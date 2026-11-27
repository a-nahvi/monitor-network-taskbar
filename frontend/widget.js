const tauri = window.__TAURI__;
const widget = document.querySelector('.widget');
const graphTransition = 230;
const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');

let lastRevision = -1;

async function subscribe(event, render) {
  const accept = (snapshot) => {
    if (snapshot.revision < lastRevision) return;
    lastRevision = snapshot.revision;
    render(snapshot);
  };
  await tauri.event.listen(event, ({ payload }) => accept(payload), {
    target: 'widget',
  });
  accept(await tauri.core.invoke('widget_snapshot'));
}

function useWidget() {
  const readings = widget.querySelector('.readings');
  const upload = widget.querySelector('#upload');
  const download = widget.querySelector('#download');
  const canvas = widget.querySelector('canvas');
  const context = canvas.getContext('2d');
  let reading;
  let scale = 1;
  let desired = [];
  let drawn = [];
  let graphFrame = 0;
  let dragFrame = 0;
  let dragging = false;

  const act = (action) => {
    void tauri.core.invoke('widget_action', { action }).catch(() => {});
  };

  function finishDrag(event, cancel = false) {
    if (!dragging) return;
    if (dragFrame) {
      cancelAnimationFrame(dragFrame);
      dragFrame = 0;
      if (!cancel) act('drag-move');
    }
    dragging = false;
    delete widget.dataset.dragging;
    act(cancel ? 'drag-cancel' : 'drag-end');
    if (widget.hasPointerCapture(event.pointerId)) widget.releasePointerCapture(event.pointerId);
  }

  widget.addEventListener('pointerdown', (event) => {
    if (!reading || reading.locked || !event.isPrimary || event.button !== 0) return;
    dragging = true;
    widget.dataset.dragging = 'true';
    widget.setPointerCapture(event.pointerId);
    act('drag-begin');
  });
  widget.addEventListener('pointermove', () => {
    if (!dragging || dragFrame) return;
    dragFrame = requestAnimationFrame(() => {
      dragFrame = 0;
      act('drag-move');
    });
  });
  widget.addEventListener('pointerup', finishDrag);
  widget.addEventListener('pointercancel', (event) => finishDrag(event, true));
  widget.addEventListener('lostpointercapture', (event) => finishDrag(event, true));
  widget.addEventListener('dblclick', () => act('settings'));
  widget.addEventListener('contextmenu', (event) => {
    event.preventDefault();
    act('context');
  });
  widget.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    event.preventDefault();
    act('settings');
  });

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) return;
    if (dragging) {
      dragging = false;
      delete widget.dataset.dragging;
      cancelAnimationFrame(dragFrame);
      dragFrame = 0;
      act('drag-cancel');
    }
  });

  function draw(points) {
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (!context || !width || !height) return;
    const density = window.devicePixelRatio || 1;
    const pixelsWide = Math.round(width * density);
    const pixelsHigh = Math.round(height * density);
    if (canvas.width !== pixelsWide) canvas.width = pixelsWide;
    if (canvas.height !== pixelsHigh) canvas.height = pixelsHigh;
    context.setTransform(density, 0, 0, density, 0, 0);
    context.clearRect(0, 0, width, height);
    const left = 1;
    const right = width - 1;
    const base = height - 4;
    const available = base - 4;
    const style = getComputedStyle(widget);
    const colors = [style.getPropertyValue('--down'), style.getPropertyValue('--up')];

    context.lineCap = 'round';
    context.lineJoin = 'round';
    context.lineWidth = 1.35;
    for (const axis of [1, 0]) {
      context.beginPath();
      context.strokeStyle = colors[axis === 1 ? 0 : 1];
      if (!points.length) {
        context.globalAlpha = 0.38;
        context.setLineDash([3, 3]);
        context.moveTo(left, base - axis * 2);
        context.lineTo(right, base - axis * 2);
      } else {
        const end = points.length - 1;
        for (let index = 0; index <= end; index += 1) {
          const x = left + (right - left) * index / (end || 1);
          const value = points[index][axis];
          const y = base - value * available;
          if (index === 0) context.moveTo(x, y);
          else context.lineTo(x, y);
        }
      }
      context.stroke();
      context.globalAlpha = 1;
      context.setLineDash([]);
    }
  }

  function drawGraph(next, resetScale) {
    cancelAnimationFrame(graphFrame);
    graphFrame = 0;
    const { connected, interval, history } = next;
    if (!connected || !history.length) {
      desired = [];
      drawn = [];
      scale = 1;
      draw(drawn);
      return;
    }
    const count = Math.floor(60_000 / interval) + 1;
    const max = Math.max(1, ...history.flat());
    if (resetScale || max > scale || max < scale * 0.64) scale = max;
    const offset = count - history.length;
    const points = Array.from({ length: count }, (_, index) => {
      const sample = history[index - offset];
      return sample ? [sample[0] / scale, sample[1] / scale] : [0, 0];
    });
    const same = (left, right) => left.length === right.length && left.every(
      ([up, down], index) => up === right[index][0] && down === right[index][1],
    );
    if (same(points, desired)) {
      drawn = desired;
      draw(drawn);
      return;
    }
    desired = points;
    if (drawn.length !== points.length || reducedMotion.matches) {
      drawn = points;
      draw(drawn);
      return;
    }
    const start = performance.now();
    const previous = drawn;
    function frame(now) {
      const elapsed = Math.min(1, (now - start) / graphTransition);
      const progress = 1 - (1 - elapsed) ** 3;
      drawn = points.map(([up, down], index) => [
        previous[index][0] + (up - previous[index][0]) * progress,
        previous[index][1] + (down - previous[index][1]) * progress,
      ]);
      draw(drawn);
      graphFrame = elapsed < 1 ? requestAnimationFrame(frame) : 0;
    }
    graphFrame = requestAnimationFrame(frame);
  }

  new ResizeObserver(() => {
    if (reading?.graph) draw(drawn);
  }).observe(widget);

  return subscribe('netbar-widget-reading', ({ reading: next }) => {
    if (!next) return;
    const resetScale = !reading?.graph || reading.interval !== next.interval;
    reading = next;
    widget.style.setProperty('--up', `rgb(${next.colors[0].join(' ')})`);
    widget.style.setProperty('--down', `rgb(${next.colors[1].join(' ')})`);
    widget.style.setProperty('--size', `${next.textSize}px`);
    widget.dataset.draggable = String(!next.locked);
    widget.setAttribute('aria-label', `Upload ${next.upload}, download ${next.download}`);
    readings.hidden = next.graph;
    canvas.hidden = !next.graph;
    upload.textContent = next.upload;
    download.textContent = next.download;
    if (next.graph) drawGraph(next, resetScale);
    else {
      cancelAnimationFrame(graphFrame);
      graphFrame = 0;
      desired = [];
      drawn = [];
    }
  });
}

if (tauri) {
  useWidget().catch(console.error);
}
