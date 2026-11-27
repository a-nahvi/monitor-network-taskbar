use std::collections::BTreeMap;
use std::mem::offset_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER,
    ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS, GetLastError, HANDLE, LocalFree, SetLastError,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_PRIVILEGE_ENABLED, SE_SYSTEM_PROFILE_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::core::GUID;

use crate::network::RateLabel;

const TCP: GUID = GUID::from_u128(0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2);
const UDP: GUID = GUID::from_u128(0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80);
const NETWORK_MODE: u32 = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
const PROCESS_LIMIT: usize = 256;
const SESSION_NAME_CAPACITY: usize = 256;
const STATUS_HEADER: &str = "Top apps · All adapters (TCP/UDP)";

pub struct ApplicationTraffic {
    collection: Option<Collection>,
    failed: bool,
}

struct Collection {
    shared: Arc<Shared>,
    cancel: Sender<()>,
    worker: JoinHandle<()>,
}

impl ApplicationTraffic {
    pub const fn new() -> Self {
        Self {
            collection: None,
            failed: false,
        }
    }

    // If a prior hover is still closing, the caller can retry at the next refresh.
    pub fn start(&mut self) -> bool {
        if self.failed {
            return true;
        }
        if let Some(previous) = &self.collection {
            if !previous.shared.stopping.load(Ordering::Acquire) {
                return true;
            }
            if !previous.worker.is_finished() {
                return false;
            }
        }
        if let Some(previous) = self.collection.take() {
            let _ = previous.worker.join();
        }
        let shared = Arc::new(Shared::default());
        let (cancel, cancellation) = mpsc::channel();
        let report = Arc::clone(&shared);
        let finish = cancel.clone();
        match thread::Builder::new()
            .name("NetBar traffic".into())
            .spawn(move || {
                collect(report, cancellation, finish);
            }) {
            Ok(worker) => {
                self.collection = Some(Collection {
                    shared,
                    cancel,
                    worker,
                })
            }
            Err(_) => self.failed = true,
        }
        true
    }

    pub fn stop(&mut self) {
        if let Some(active) = &self.collection
            && !active.shared.stopping.swap(true, Ordering::AcqRel)
        {
            let _ = active.cancel.send(());
        }
        self.failed = false;
    }

    pub fn summary(&self, bits: bool) -> String {
        use std::fmt::Write;

        let mut output = String::from(STATUS_HEADER);
        let snapshot = self
            .collection
            .as_ref()
            .filter(|active| !active.shared.stopping.load(Ordering::Acquire))
            .map(|active| lock(&active.shared.published));
        match snapshot.as_deref() {
            Some(Snapshot::AccessDenied) => {
                output.push_str("\nRun NetBar as administrator for per-app measurement.");
            }
            Some(Snapshot::Unavailable) => output.push_str("\nPer-app measurement is unavailable."),
            Some(Snapshot::Ready {
                applications,
                incomplete,
            }) => {
                for app in applications {
                    let _ = write!(
                        output,
                        "\n{}  ↑ {}   ↓ {}",
                        app.name,
                        RateLabel::new(app.upload, bits).as_str(),
                        RateLabel::new(app.download, bits).as_str()
                    );
                }
                if applications.is_empty() && !incomplete {
                    output.push_str("\nNo app traffic in the latest sample.");
                }
                if *incomplete {
                    output.push_str("\nPartial measurement. Some events were unavailable.");
                }
            }
            _ if self.failed => output.push_str("\nPer-app measurement is unavailable."),
            _ => output.push_str("\nMeasuring app traffic…"),
        }
        output
    }
}

impl Drop for ApplicationTraffic {
    fn drop(&mut self) {
        self.stop();
        if let Some(active) = self.collection.take() {
            let _ = active.worker.join();
        }
    }
}

#[derive(Default)]
struct Shared {
    stopping: AtomicBool,
    totals: Mutex<Accounting>,
    published: Mutex<Snapshot>,
}

#[derive(Default)]
enum Snapshot {
    #[default]
    Starting,
    AccessDenied,
    Unavailable,
    Ready {
        applications: Vec<ApplicationRate>,
        incomplete: bool,
    },
}

struct ApplicationRate {
    name: String,
    upload: f64,
    download: f64,
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    pid: u32,
    bytes: [u64; 2],
}

struct Accounting {
    processes: [Bucket; PROCESS_LIMIT],
    incomplete: bool,
    unknown_schema: bool,
}

impl Default for Accounting {
    fn default() -> Self {
        Self {
            processes: [Bucket::default(); PROCESS_LIMIT],
            incomplete: false,
            unknown_schema: false,
        }
    }
}

impl Accounting {
    fn add(&mut self, pid: u32, bytes: u32, download: bool) {
        if bytes == 0 {
            return;
        }
        if pid == 0 {
            self.incomplete = true;
            return;
        }
        let start = (pid as usize).wrapping_mul(2_654_435_761);
        for probe in 0..PROCESS_LIMIT {
            let bucket = &mut self.processes[(start.wrapping_add(probe)) % PROCESS_LIMIT];
            if bucket.pid != 0 && bucket.pid != pid {
                continue;
            }
            bucket.pid = pid;
            let total = &mut bucket.bytes[usize::from(download)];
            let (sum, overflow) = total.overflowing_add(u64::from(bytes));
            *total = if overflow { u64::MAX } else { sum };
            self.incomplete |= overflow;
            return;
        }
        self.incomplete = true;
    }

    fn drain(&mut self) -> ([Bucket; PROCESS_LIMIT], bool) {
        let processes = std::mem::replace(&mut self.processes, [Bucket::default(); PROCESS_LIMIT]);
        let incomplete = std::mem::take(&mut self.incomplete) || self.unknown_schema;
        (processes, incomplete)
    }
}

#[derive(Clone, Copy, Default)]
struct Schema {
    key: u64,
    valid: bool,
}

struct CallbackContext {
    shared: Arc<Shared>,
    schemas: [Schema; 32],
    schema_count: usize,
}

unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    // Only the consumer accesses the context while the controller keeps its allocation live.
    let Some(context) = (unsafe { record.UserContext.cast::<CallbackContext>().as_mut() }) else {
        return;
    };
    if context.shared.stopping.load(Ordering::Relaxed) {
        return;
    }
    let event = record.EventHeader;
    let udp = same_guid(event.ProviderId, UDP);
    if !udp && !same_guid(event.ProviderId, TCP) {
        return;
    }
    let descriptor = event.EventDescriptor;
    let download = match descriptor.Opcode {
        10 | 26 => false,
        11 | 27 => true,
        _ => return,
    };
    let key = (u64::from(udp) << 40)
        | (u64::from(descriptor.Version) << 32)
        | (u64::from(descriptor.Id) << 8)
        | u64::from(descriptor.Opcode);
    let valid = if let Some(schema) = context.schemas[..context.schema_count]
        .iter()
        .find(|item| item.key == key)
    {
        schema.valid
    } else if context.schema_count < context.schemas.len() {
        let valid = byte_prefix(record);
        context.schemas[context.schema_count] = Schema { key, valid };
        context.schema_count += 1;
        valid
    } else {
        false
    };
    let mut totals = lock(&context.shared.totals);
    if valid && !record.UserData.is_null() && record.UserDataLength >= 8 {
        // TDH confirmed the first two scalar fields, which may be unaligned in a trace buffer.
        let bytes = record.UserData.cast::<u32>();
        unsafe {
            totals.add(
                bytes.read_unaligned(),
                bytes.add(1).read_unaligned(),
                download,
            )
        };
    } else {
        totals.unknown_schema = true;
    }
}

fn byte_prefix(record: &EVENT_RECORD) -> bool {
    let properties = offset_of!(TRACE_EVENT_INFO, EventPropertyInfoArray);
    let minimum = properties + 2 * size_of::<EVENT_PROPERTY_INFO>();
    let mut size = 0;
    unsafe {
        if TdhGetEventInformation(record, 0, null(), null_mut(), &mut size)
            != ERROR_INSUFFICIENT_BUFFER
            || !(minimum..=65_536).contains(&(size as usize))
        {
            return false;
        }
        let mut storage = vec![0u64; (size as usize).div_ceil(size_of::<u64>())];
        let info = storage.as_mut_ptr().cast::<TRACE_EVENT_INFO>();
        if TdhGetEventInformation(record, 0, null(), info, &mut size) != ERROR_SUCCESS
            || (size as usize) < minimum
            || size as usize > storage.len() * size_of::<u64>()
            || (*info).TopLevelPropertyCount < 2
        {
            return false;
        }
        let bytes = storage.as_ptr().cast::<u8>();
        let fields = bytes.add(properties).cast::<EVENT_PROPERTY_INFO>();
        let metadata = std::slice::from_raw_parts(bytes.cast::<u16>(), size as usize / 2);
        property_matches(&*fields, metadata, b"pid")
            && property_matches(&*fields.add(1), metadata, b"size")
    }
}

fn property_matches(field: &EVENT_PROPERTY_INFO, metadata: &[u16], expected: &[u8]) -> bool {
    let offset = field.NameOffset as usize;
    if field.Flags != 0 || !offset.is_multiple_of(2) {
        return false;
    }
    let start = offset / 2;
    let Some(name) = metadata.get(start..start + expected.len() + 1) else {
        return false;
    };
    name[expected.len()] == 0
        && name[..expected.len()]
            .iter()
            .zip(expected)
            .all(|(&actual, &expected)| {
                u8::try_from(actual).is_ok_and(|actual| actual.eq_ignore_ascii_case(&expected))
            })
        && unsafe {
            field.Anonymous2.count == 1
                && field.Anonymous1.nonStructType.InType == TDH_INTYPE_UINT32 as u16
        }
}

fn collect(shared: Arc<Shared>, cancellation: Receiver<()>, finish: Sender<()>) {
    if shared.stopping.load(Ordering::Acquire) {
        return;
    }
    let Some(mut name) = session_identity() else {
        *lock(&shared.published) = Snapshot::Unavailable;
        return;
    };
    // A separate per-user guard also covers directly launched Rust developer copies.
    let mutex_name: Vec<_> = "Local\\"
        .encode_utf16()
        .chain(name.iter().copied())
        .collect();
    let mutex = OwnedHandle(unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) });
    if mutex.0.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        *lock(&shared.published) = Snapshot::Unavailable;
        return;
    }
    let _privilege = ProfilePrivilege::enable();
    let mut session = match TraceSession::start(&name) {
        Ok(session) => session,
        Err(ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD) => {
            *lock(&shared.published) = Snapshot::AccessDenied;
            return;
        }
        Err(_) => {
            *lock(&shared.published) = Snapshot::Unavailable;
            return;
        }
    };
    if shared.stopping.load(Ordering::Acquire) {
        return;
    }
    let mut context = CallbackContext {
        shared: Arc::clone(&shared),
        schemas: [Schema::default(); 32],
        schema_count: 0,
    };
    let mut logfile = EVENT_TRACE_LOGFILEW {
        LoggerName: name.as_mut_ptr(),
        Anonymous1: EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD,
        },
        Anonymous2: EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(event_callback),
        },
        Context: (&raw mut context).cast(),
        ..Default::default()
    };
    let reader = unsafe { OpenTraceW(&mut logfile) };
    if reader.Value == u64::MAX {
        *lock(&shared.published) = match unsafe { GetLastError() } {
            ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD => Snapshot::AccessDenied,
            _ => Snapshot::Unavailable,
        };
        return;
    }
    // The scoped consumer completes before `context` and the logger name can drop.
    let reader = TraceReader(reader);
    thread::scope(move |scope| {
        let handle = reader.0;
        let report = Arc::clone(&shared);
        let dispatch = thread::Builder::new()
            .name("NetBar trace reader".into())
            .spawn_scoped(scope, move || {
                unsafe { ProcessTrace(&handle, 1, null(), null()) };
                if !report.stopping.load(Ordering::Acquire) {
                    *lock(&report.published) = Snapshot::Unavailable;
                    let _ = finish.send(());
                }
            });
        if dispatch.is_ok() {
            let mut previous = Instant::now();
            while let Err(RecvTimeoutError::Timeout) =
                cancellation.recv_timeout(Duration::from_secs(1))
            {
                let now = Instant::now();
                publish(&shared, now.duration_since(previous), session.lost_events());
                previous = now;
            }
        } else {
            *lock(&shared.published) = Snapshot::Unavailable;
        }
        drop(session);
        drop(reader);
    });
}

struct TraceReader(PROCESSTRACE_HANDLE);

impl Drop for TraceReader {
    fn drop(&mut self) {
        unsafe { CloseTrace(self.0) };
    }
}

fn publish(shared: &Shared, elapsed: Duration, lost_events: bool) {
    let (processes, incomplete) = lock(&shared.totals).drain();
    let elapsed = elapsed.as_secs_f64();
    let mut grouped = BTreeMap::<String, ApplicationRate>::new();
    let mut path = [0u16; 32768];
    if elapsed > 0.0 {
        for process in processes.into_iter().filter(|process| process.pid != 0) {
            let name = process_name(process.pid, &mut path);
            let rate = grouped
                .entry(name.to_lowercase())
                .or_insert(ApplicationRate {
                    name,
                    upload: 0.0,
                    download: 0.0,
                });
            rate.upload += process.bytes[0] as f64 / elapsed;
            rate.download += process.bytes[1] as f64 / elapsed;
        }
    }
    let mut applications: Vec<_> = grouped.into_values().collect();
    applications.sort_by(|a, b| (b.upload + b.download).total_cmp(&(a.upload + a.download)));
    applications.truncate(5);
    let mut published = lock(&shared.published);
    if !matches!(*published, Snapshot::Unavailable) {
        *published = Snapshot::Ready {
            applications,
            incomplete: incomplete || lost_events,
        };
    }
}

fn process_name(pid: u32, buffer: &mut [u16]) -> String {
    if pid == 4 {
        return "System".into();
    }
    let process = OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    let mut length = buffer.len() as u32;
    if process.0.is_null()
        || unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) }
            == 0
        || length == 0
    {
        return format!("PID {pid}");
    }
    let basename = buffer[..length as usize]
        .rsplit(|&unit| unit == b'\\' as u16)
        .next()
        .unwrap_or_default();
    if basename.len() > 40 {
        format!("{}…", String::from_utf16_lossy(&basename[..39]))
    } else {
        String::from_utf16_lossy(basename)
    }
}

#[repr(C)]
struct TraceProperties {
    value: EVENT_TRACE_PROPERTIES,
    name: [u16; SESSION_NAME_CAPACITY],
}

impl TraceProperties {
    fn new(guid: GUID) -> Self {
        let mut value = EVENT_TRACE_PROPERTIES::default();
        value.Wnode.BufferSize = size_of::<Self>() as u32;
        value.Wnode.Guid = guid;
        value.Wnode.ClientContext = 1;
        value.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        value.BufferSize = 64;
        value.MinimumBuffers = 2;
        value.MaximumBuffers = 8;
        value.FlushTimer = 1;
        value.LogFileMode = NETWORK_MODE | EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING;
        value.EnableFlags = EVENT_TRACE_FLAG_NETWORK_TCPIP;
        value.LoggerNameOffset = offset_of!(Self, name) as u32;
        Self {
            value,
            name: [0; SESSION_NAME_CAPACITY],
        }
    }
}

struct TraceSession {
    handle: CONTROLTRACE_HANDLE,
    properties: TraceProperties,
}

impl TraceSession {
    fn start(name: &[u16]) -> Result<Self, u32> {
        let guid = session_guid(&name[..name.len() - 1]);
        let mut properties = TraceProperties::new(guid);
        let mut handle = CONTROLTRACE_HANDLE::default();
        let mut status = unsafe { StartTraceW(&mut handle, name.as_ptr(), &mut properties.value) };
        if status == ERROR_ALREADY_EXISTS {
            let empty = CONTROLTRACE_HANDLE::default();
            let mut orphan = TraceProperties::new(guid);
            // Inspect the exact legacy NetBar identity and scope while the collector guard is held.
            if unsafe {
                ControlTraceW(
                    empty,
                    name.as_ptr(),
                    &mut orphan.value,
                    EVENT_TRACE_CONTROL_QUERY,
                )
            } == ERROR_SUCCESS
                && same_guid(orphan.value.Wnode.Guid, guid)
                && orphan.value.EnableFlags == EVENT_TRACE_FLAG_NETWORK_TCPIP
                && orphan.value.LogFileNameOffset == 0
                && orphan.value.LogFileMode & NETWORK_MODE == NETWORK_MODE
                && unsafe {
                    ControlTraceW(
                        empty,
                        name.as_ptr(),
                        &mut orphan.value,
                        EVENT_TRACE_CONTROL_STOP,
                    )
                } == ERROR_SUCCESS
            {
                properties = TraceProperties::new(guid);
                status = unsafe { StartTraceW(&mut handle, name.as_ptr(), &mut properties.value) };
            }
        }
        if status == ERROR_SUCCESS {
            Ok(Self { handle, properties })
        } else {
            Err(status)
        }
    }

    fn lost_events(&mut self) -> bool {
        let status = unsafe {
            ControlTraceW(
                self.handle,
                null(),
                &mut self.properties.value,
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        status != ERROR_SUCCESS
            || self.properties.value.EventsLost > 0
            || self.properties.value.RealTimeBuffersLost > 0
    }
}

impl Drop for TraceSession {
    fn drop(&mut self) {
        unsafe {
            ControlTraceW(
                self.handle,
                null(),
                &mut self.properties.value,
                EVENT_TRACE_CONTROL_STOP,
            )
        };
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct ProfilePrivilege {
    token: OwnedHandle,
    previous: TOKEN_PRIVILEGES,
    restore: bool,
}

impl ProfilePrivilege {
    fn enable() -> Option<Self> {
        unsafe {
            let mut token = OwnedHandle(null_mut());
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                &mut token.0,
            ) == 0
            {
                return None;
            }
            let mut luid = Default::default();
            if LookupPrivilegeValueW(null(), SE_SYSTEM_PROFILE_NAME, &mut luid) == 0 {
                return None;
            }
            let request = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            let mut bytes = size_of::<TOKEN_PRIVILEGES>() as u32;
            SetLastError(ERROR_SUCCESS);
            let restore =
                AdjustTokenPrivileges(token.0, 0, &request, bytes, &mut previous, &mut bytes) != 0
                    && GetLastError() == ERROR_SUCCESS;
            Some(Self {
                token,
                previous,
                restore,
            })
        }
    }
}

impl Drop for ProfilePrivilege {
    fn drop(&mut self) {
        if self.restore {
            unsafe {
                AdjustTokenPrivileges(self.token.0, 0, &self.previous, 0, null_mut(), null_mut())
            };
        }
    }
}

fn session_identity() -> Option<Vec<u16>> {
    unsafe {
        let mut token = OwnedHandle(null_mut());
        let mut session_id = 0;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token.0) == 0
            || ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) == 0
        {
            return None;
        }
        let mut bytes = 0;
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut bytes);
        if !(size_of::<TOKEN_USER>()..=65_536).contains(&(bytes as usize)) {
            return None;
        }
        let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        ) == 0
        {
            return None;
        }
        let mut sid = null_mut();
        if ConvertSidToStringSidW((*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut sid) == 0
            || sid.is_null()
        {
            return None;
        }
        let mut name: Vec<_> = "NetBar.ApplicationTraffic.".encode_utf16().collect();
        let mut position = sid;
        while *position != 0 {
            name.push(*position);
            position = position.add(1);
        }
        LocalFree(sid.cast());
        name.extend(format!(".Session{session_id}").encode_utf16());
        name.push(0);
        (name.len() <= SESSION_NAME_CAPACITY).then_some(name)
    }
}

fn session_guid(name: &[u16]) -> GUID {
    let (mut first, mut second) = (
        14_695_981_039_346_656_037_u64,
        7_809_847_782_465_536_322_u64,
    );
    for &character in name {
        first = (first ^ u64::from(character)).wrapping_mul(1_099_511_628_211);
        second = (second ^ u64::from(character)).wrapping_mul(14_029_467_366_897_019_727);
    }
    let mut data4 = second.to_le_bytes();
    data4[0] = (data4[0] & 0x3f) | 0x80;
    GUID {
        data1: first as u32,
        data2: (first >> 32) as u16,
        data3: ((first >> 48) as u16 & 0x0fff) | 0x4000,
        data4,
    }
}

fn same_guid(a: GUID, b: GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_both_directions_and_drains() {
        let mut usage = Accounting::default();
        usage.add(42, 33, false);
        usage.add(42, 13, false);
        usage.add(42, 21, true);
        usage.add(42 + 256, 100, false);
        let (processes, incomplete) = usage.drain();
        assert!(!incomplete);
        assert_eq!(
            processes.iter().find(|item| item.pid == 42).unwrap().bytes,
            [46, 21]
        );
        assert_eq!(
            processes
                .iter()
                .find(|item| item.pid == 42 + 256)
                .unwrap()
                .bytes,
            [100, 0]
        );
        assert!(usage.drain().0.iter().all(|item| item.pid == 0));
    }

    #[test]
    fn reports_missing_pids_and_capacity_without_growing() {
        let mut usage = Accounting::default();
        for pid in 1..=PROCESS_LIMIT as u32 {
            usage.add(pid, 1, false);
        }
        assert!(!usage.incomplete);
        usage.add(1234, 1, false);
        assert!(usage.drain().1);
        usage.add(0, 250, true);
        assert!(usage.drain().1);
        assert!(!usage.drain().1);
    }
}
