use std::{
    ffi::c_void,
    fmt::{self, Write},
    ptr::{addr_of, null_mut},
    slice,
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::{HANDLE, HWND},
        NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
        Networking::WinSock::AF_UNSPEC,
        UI::WindowsAndMessaging::PostMessageW,
    },
    core::GUID,
};

#[derive(Clone, Debug)]
pub struct Adapter {
    pub id: String,
    pub name: String,
    pub connected: bool,
    luid: u64,
    hardware: bool,
    route: Option<u64>,
}

fn text_at(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())])
}

fn guid_text(guid: GUID) -> String {
    let [a, b, c, d, e, f, g, h] = guid.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{a:02X}{b:02X}-{c:02X}{d:02X}{e:02X}{f:02X}{g:02X}{h:02X}}}",
        guid.data1, guid.data2, guid.data3
    )
}

const HARDWARE: u8 = 1;
const FILTER: u8 = 2;
const MEDIA_DISCONNECTED: u8 = 16;
const PAUSED: u8 = 32;

fn is_connected(row: &MIB_IF_ROW2) -> bool {
    row.OperStatus == IfOperStatusUp
        && row.InterfaceAndOperStatusFlags._bitfield & (MEDIA_DISCONNECTED | PAUSED) == 0
}

pub fn adapters() -> Vec<Adapter> {
    unsafe {
        let mut table = null_mut();
        if GetIfTable2(&mut table) != 0 || table.is_null() {
            return Vec::new();
        }
        let rows = slice::from_raw_parts(
            addr_of!((*table).Table).cast(),
            (*table).NumEntries as usize,
        );
        let mut adapters: Vec<_> = rows
            .iter()
            .filter_map(|row: &MIB_IF_ROW2| {
                let flags = row.InterfaceAndOperStatusFlags._bitfield;
                if flags & FILTER != 0 || row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                    return None;
                }
                Some(Adapter {
                    id: guid_text(row.InterfaceGuid),
                    name: text_at(if row.Alias[0] == 0 {
                        &row.Description
                    } else {
                        &row.Alias
                    }),
                    connected: is_connected(row),
                    luid: row.InterfaceLuid.Value,
                    hardware: flags & HARDWARE != 0,
                    route: None,
                })
            })
            .collect();
        FreeMibTable(table.cast());

        let mut routes = null_mut();
        if GetIpForwardTable2(AF_UNSPEC, &mut routes) == 0 && !routes.is_null() {
            let rows = slice::from_raw_parts(
                addr_of!((*routes).Table).cast(),
                (*routes).NumEntries as usize,
            );
            for route in rows
                .iter()
                .filter(|row: &&MIB_IPFORWARD_ROW2| row.DestinationPrefix.PrefixLength == 0)
            {
                if let Some(adapter) = adapters
                    .iter_mut()
                    .find(|item| item.luid == route.InterfaceLuid.Value)
                {
                    let mut ip = MIB_IPINTERFACE_ROW {
                        Family: route.DestinationPrefix.Prefix.si_family,
                        InterfaceLuid: route.InterfaceLuid,
                        ..Default::default()
                    };
                    let extra = if GetIpInterfaceEntry(&mut ip) == 0 {
                        ip.Metric
                    } else {
                        0
                    };
                    let cost = u64::from(route.Metric) + u64::from(extra);
                    adapter.route = Some(adapter.route.map_or(cost, |previous| previous.min(cost)));
                }
            }
            FreeMibTable(routes.cast());
        }
        adapters.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        adapters
    }
}

pub fn select(items: &[Adapter], preferred: &str) -> Option<u64> {
    if !preferred.is_empty() {
        return items
            .iter()
            .find(|item| item.connected && item.id.eq_ignore_ascii_case(preferred))
            .map(|item| item.luid);
    }
    items
        .iter()
        .filter(|item| item.connected)
        .min_by_key(|item| {
            (
                !item.hardware,
                item.route.is_none(),
                item.route.unwrap_or(u64::MAX),
                item.luid,
            )
        })
        .map(|item| item.luid)
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Rates {
    pub up: f64,
    pub down: f64,
    pub connected: bool,
}

#[derive(Default)]
pub struct Monitor {
    selected: Option<u64>,
    previous: Option<(Instant, u64, u64)>,
}

impl Monitor {
    pub fn select(&mut self, luid: Option<u64>) -> bool {
        if self.selected == luid {
            return false;
        }
        self.selected = luid;
        self.reset();
        true
    }

    pub fn reset(&mut self) {
        self.previous = None;
    }

    pub fn sample(&mut self) -> Rates {
        let Some(luid) = self.selected else {
            return Rates::default();
        };
        let mut row = MIB_IF_ROW2::default();
        row.InterfaceLuid.Value = luid;
        if unsafe { GetIfEntry2(&mut row) } != 0 || !is_connected(&row) {
            self.reset();
            return Rates::default();
        }
        let now = Instant::now();
        let previous = self.previous.replace((now, row.OutOctets, row.InOctets));
        let mut rates = Rates {
            connected: true,
            ..Rates::default()
        };
        if let Some((time, up, down)) = previous {
            let elapsed = now.duration_since(time);
            if !elapsed.is_zero()
                && elapsed < Duration::from_secs(30)
                && let (Some(up), Some(down)) = (
                    row.OutOctets.checked_sub(up),
                    row.InOctets.checked_sub(down),
                )
            {
                rates.up = up as f64 / elapsed.as_secs_f64();
                rates.down = down as f64 / elapsed.as_secs_f64();
            }
        }
        rates
    }
}

const NETWORK_CHANGED: u32 = crate::NETWORK_CHANGED;

pub struct Notifications {
    interface: HANDLE,
    route: HANDLE,
}

impl Notifications {
    pub fn new(window: HWND) -> Self {
        let mut result = Self {
            interface: null_mut(),
            route: null_mut(),
        };
        unsafe {
            if NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(interface_changed),
                window,
                false,
                &mut result.interface,
            ) != 0
            {
                result.interface = null_mut();
            }
            if NotifyRouteChange2(
                AF_UNSPEC,
                Some(route_changed),
                window,
                false,
                &mut result.route,
            ) != 0
            {
                result.route = null_mut();
            }
        }
        result
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        unsafe {
            for registration in [self.interface, self.route] {
                if !registration.is_null() {
                    CancelMibChangeNotify2(registration);
                }
            }
        }
    }
}

unsafe extern "system" fn interface_changed(
    window: *const c_void,
    _: *const MIB_IPINTERFACE_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    unsafe {
        PostMessageW(window.cast_mut(), NETWORK_CHANGED, 0, 0);
    }
}

unsafe extern "system" fn route_changed(
    window: *const c_void,
    row: *const MIB_IPFORWARD_ROW2,
    _: MIB_NOTIFICATION_TYPE,
) {
    if row.is_null() || unsafe { (*row).DestinationPrefix.PrefixLength } == 0 {
        unsafe {
            PostMessageW(window.cast_mut(), NETWORK_CHANGED, 0, 0);
        }
    }
}

pub struct RateLabel {
    bytes: [u8; 40],
    count: usize,
}

impl RateLabel {
    pub fn new(rate: f64, bits: bool) -> Self {
        let mut label = Self {
            bytes: [0; 40],
            count: 0,
        };
        let rate = if bits { rate * 8.0 } else { rate };
        let mut rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            0.0
        };
        let base = if bits { 1000.0 } else { 1024.0 };
        let labels = if bits {
            ["b/s", "Kb/s", "Mb/s", "Gb/s", "Tb/s"]
        } else {
            ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"]
        };
        let mut unit = 0;
        while unit < labels.len() - 1 && rate >= base {
            rate /= base;
            unit += 1;
        }
        if unit < labels.len() - 1 && rate.round() >= base {
            rate /= base;
            unit += 1;
        }
        let precision = usize::from(unit != 0 && rate < 100.0);
        let _ = write!(label, "{rate:.precision$} {}", labels[unit]);
        label
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.count]).unwrap_or("")
    }
}

impl Write for RateLabel {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.count + s.len();
        self.bytes
            .get_mut(self.count..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.count = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_round_across_boundaries() {
        assert_eq!(RateLabel::new(1023.8, false).as_str(), "1.0 KB/s");
        assert_eq!(RateLabel::new(125.0, true).as_str(), "1.0 Kb/s");
        assert_eq!(RateLabel::new(f64::NAN, false).as_str(), "0 B/s");
    }

    #[test]
    fn auto_avoids_double_counting_and_manual_does_not_switch() {
        let adapter = |name: &str, luid, hardware, route| Adapter {
            id: name.into(),
            name: name.into(),
            connected: true,
            luid,
            hardware,
            route,
        };
        let items = [
            adapter("VPN", 1, false, Some(1)),
            adapter("Wi-Fi", 2, true, None),
            adapter("Ethernet", 3, true, Some(10)),
        ];
        assert_eq!(select(&items, ""), Some(3));
        assert_eq!(select(&items, "vpn"), Some(1));
        assert_eq!(select(&items, "missing"), None);
    }
}
