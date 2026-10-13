import { useRef, useSyncExternalStore } from 'react';
import { createRoot } from 'react-dom/client';
import { Dialog } from '@base-ui/react/dialog';
import { Command } from 'cmdk';

export interface PaletteItem {
  g: string;
  t: string;
  i: string;
  d?: string;
  sub?: string;
  k?: string;
  run: () => void;
}

interface OverlayOptions {
  host: HTMLElement;
  label: string;
  closeLabel: string;
  onClosed: () => void;
}

export interface PaletteOptions extends OverlayOptions {
  items: PaletteItem[];
  placeholder: string;
  empty: string;
  hints: [string, string, string];
}

export interface ImageOptions extends OverlayOptions {
  src: string;
  caption: string;
}

interface Entry<T> {
  generation: number;
  open: boolean;
  options: T;
  returnTo: HTMLElement | null;
}

interface Snapshot {
  palette: Entry<PaletteOptions> | null;
  image: Entry<ImageOptions> | null;
}

let snapshot: Snapshot = { palette: null, image: null };
const listeners = new Set<() => void>();
let mounted = false;
let generation = 0;

function update(next: Snapshot) {
  snapshot = next;
  listeners.forEach(listener => listener());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

function useOverlays() {
  return useSyncExternalStore(subscribe, () => snapshot);
}

function close(kind: keyof Snapshot) {
  const entry = snapshot[kind];
  if (entry?.open) update({ ...snapshot, [kind]: { ...entry, open: false } });
}

function complete(kind: keyof Snapshot, entry: Entry<OverlayOptions>, open: boolean) {
  // A late transition callback must never hide an overlay that was reopened.
  if (open || snapshot[kind] !== entry || entry.open) return;
  entry.options.host.hidden = true;
  entry.options.onClosed();
}

function Icon({ name }: { name: string }) {
  return <svg className="i" aria-hidden="true"><use href={`#i-${name}`} /></svg>;
}

function Palette({ entry }: { entry: Entry<PaletteOptions> }) {
  const input = useRef<HTMLInputElement>(null);
  const { options, open, returnTo } = entry;
  const groups = new Map<string, Array<PaletteItem & { value: string }>>();
  options.items.forEach((item, index) => {
    const rows = groups.get(item.g) ?? [];
    rows.push({ ...item, value: String(index) });
    groups.set(item.g, rows);
  });
  return <Dialog.Root open={open} onOpenChange={next => { if (!next) close('palette'); }} onOpenChangeComplete={next => complete('palette', entry, next)}>
    <Dialog.Portal container={options.host}>
      <Dialog.Backdrop className="scrim" />
      <Dialog.Popup className="box glass" initialFocus={input} finalFocus={() => returnTo?.isConnected ? returnTo : false} aria-modal="true" aria-describedby={undefined}>
        <Dialog.Title className="sr-only">{options.label}</Dialog.Title>
        <Command label={options.label} loop>
          <div className="in"><Icon name="search" /><Command.Input ref={input} id="pal-in" placeholder={options.placeholder} autoComplete="off" spellCheck={false} /><Dialog.Close className="ib sm" aria-label={options.closeLabel}><Icon name="x" /></Dialog.Close></div>
          <Command.List id="pal-list" className="plist scroll">
            <Command.Empty className="none">{options.empty}</Command.Empty>
            {[...groups].map(([group, items]) => <Command.Group key={group} heading={group}>
              {items.map(item => <Command.Item key={item.value} value={item.value} keywords={[item.t, item.g, item.d ?? '', item.sub ?? '']} className="mi" onSelect={() => { close('palette'); item.run(); }}>
                <Icon name={item.i} /><span className="mt"><span>{item.t}</span>{item.sub && <small>{item.sub}</small>}</span>{item.d && <kbd>{item.d}</kbd>}{item.k && <kbd>{item.k}</kbd>}
              </Command.Item>)}
            </Command.Group>)}
          </Command.List>
          <div className="pf2"><span><kbd>↑↓</kbd>{options.hints[0]}</span><span><kbd>Enter</kbd>{options.hints[1]}</span><span><kbd>Esc</kbd>{options.hints[2]}</span></div>
        </Command>
      </Dialog.Popup>
    </Dialog.Portal>
  </Dialog.Root>;
}

function ImageViewer({ entry }: { entry: Entry<ImageOptions> }) {
  const { options, open, returnTo } = entry;
  return <Dialog.Root open={open} onOpenChange={next => { if (!next) close('image'); }} onOpenChangeComplete={next => complete('image', entry, next)}>
    <Dialog.Portal container={options.host}>
      <Dialog.Backdrop className="scrim" />
      <Dialog.Popup className="stage" finalFocus={() => returnTo?.isConnected ? returnTo : false} aria-modal="true" aria-describedby={undefined}>
        <Dialog.Title className="sr-only">{options.label}</Dialog.Title>
        <Dialog.Close className="imv-close ib" aria-label={options.closeLabel}><Icon name="x" /></Dialog.Close>
        <Dialog.Close className="imv-image" aria-label={options.closeLabel}><img id="imv-img" src={options.src} alt={options.caption} /></Dialog.Close>
        {options.caption && <p id="imv-cap">{options.caption}</p>}
      </Dialog.Popup>
    </Dialog.Portal>
  </Dialog.Root>;
}

function Overlays() {
  const state = useOverlays();
  return <>{state.palette && <Palette key={state.palette.generation} entry={state.palette} />}{state.image && <ImageViewer key={state.image.generation} entry={state.image} />}</>;
}

function ensureMounted() {
  if (mounted) return;
  const host = document.getElementById('ecosystem-root');
  if (!host) throw new Error('Missing ecosystem overlay root');
  createRoot(host).render(<Overlays />);
  mounted = true;
}

export function openPalette(options: PaletteOptions) {
  ensureMounted();
  options.host.hidden = false;
  update({ ...snapshot, palette: { generation: ++generation, open: true, options, returnTo: document.activeElement instanceof HTMLElement ? document.activeElement : null } });
}

export function closePalette() { close('palette'); }

export function openImage(options: ImageOptions) {
  ensureMounted();
  options.host.hidden = false;
  update({ ...snapshot, image: { generation: ++generation, open: true, options, returnTo: document.activeElement instanceof HTMLElement ? document.activeElement : null } });
}

export function closeImage() { close('image'); }
