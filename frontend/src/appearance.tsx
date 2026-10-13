import { useEffect, useState } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { createPortal } from 'react-dom';
import { Switch } from '@base-ui/react/switch';
import { Slider } from '@base-ui/react/slider';
import { Select } from '@base-ui/react/select';
import { Radio } from '@base-ui/react/radio';
import { RadioGroup } from '@base-ui/react/radio-group';

type Key = 'mode' | 'side' | 'opacity' | 'blur' | 'dim' | 'contrast' | 'fontui' | 'fontcode' | 'motion' | 'lang';
type Value = string | number | boolean;
interface ChoiceItem { value: string; label: string; description?: string }
export interface AppearanceOptions {
  hosts: Record<'sliders' | 'themes' | 'side' | 'contrast' | 'fontui' | 'fontcode' | 'motion' | 'lang', HTMLElement>;
  values: Record<Key, Value>;
  labels: Record<Key, string>;
  choices: Record<'mode' | 'fontui' | 'fontcode' | 'motion' | 'lang', ChoiceItem[]>;
  systemTheme: string;
  gap: number;
  onChange: (key: Key, value: Value) => void;
  onOverlay: (id: string, open: boolean) => void;
}

function Choice({ kind, options }: { kind: 'fontui' | 'fontcode' | 'motion' | 'lang'; options: AppearanceOptions }) {
  const [open, setOpen] = useState(false);
  useEffect(() => () => options.onOverlay(kind, false), []);
  return <Select.Root value={String(options.values[kind])} items={options.choices[kind]} open={open} onOpenChange={next => { setOpen(next); if (next) options.onOverlay(kind, true); }} onOpenChangeComplete={next => { if (!next) options.onOverlay(kind, false); }} onValueChange={value => { if (value !== null) options.onChange(kind, value); }}>
    <Select.Trigger className="pill plain" aria-label={options.labels[kind]} data-ecosystem=""><Select.Value /><Select.Icon><svg className="i" aria-hidden="true"><use href="#i-chev-d" /></svg></Select.Icon></Select.Trigger>
    <Select.Portal><Select.Positioner className="ui-select-positioner" sideOffset={options.gap} align="end" alignItemWithTrigger={false}>
      <Select.Popup className="pop glass ui-select-popup" data-ecosystem=""><Select.List className="mp-list scroll">
        {options.choices[kind].map(item => <Select.Item key={item.value} value={item.value} className="mi">
          <Select.ItemIndicator><svg className="i sm ck" aria-hidden="true"><use href="#i-check" /></svg></Select.ItemIndicator><Select.ItemText className="mt"><span>{item.label}</span>{item.description && <small>{item.description}</small>}</Select.ItemText>
        </Select.Item>)}
      </Select.List></Select.Popup>
    </Select.Positioner></Select.Portal>
  </Select.Root>;
}

function Range({ kind, min, max, options }: { kind: 'opacity' | 'blur' | 'dim' | 'contrast'; min: number; max: number; options: AppearanceOptions }) {
  const value = Math.max(min, Math.min(max, Math.round(Number(options.values[kind]))));
  const format = (value: number) => kind === 'blur' ? `${value}px` : kind === 'contrast' ? String(value) : `${value}%`;
  return <div className="sl" data-ecosystem=""><label>{kind !== 'contrast' && options.labels[kind]}<output>{format(value)}</output></label>
    <Slider.Root value={value} min={min} max={max} step={1} onValueChange={next => options.onChange(kind, next)}>
      <Slider.Control className="ui-slider-control"><Slider.Track className="ui-slider-track"><Slider.Indicator className="ui-slider-fill" /><Slider.Thumb className="ui-slider-thumb" getAriaLabel={() => options.labels[kind]} getAriaValueText={(_, value) => format(value)} /></Slider.Track></Slider.Control>
    </Slider.Root>
  </div>;
}

function MiniTheme({ mode }: { mode: string }) {
  const half = (theme: string, second = false) => <span className={`half ${theme}${second ? ' b' : ''}`}><span className="m-bg" /><span className="m-sb" /><span className="m-ln line-1" /><span className="m-ln line-2" /><span className="m-ln line-3" /><span className="m-cp" /></span>;
  return <span className="mini">{mode === 'system' ? <>{half('dk')}{half('lt', true)}</> : half(mode === 'dark' ? 'dk' : 'lt')}</span>;
}

function Appearance({ options }: { options: AppearanceOptions }) {
  const h = options.hosts;
  return <>
    {createPortal(<><Range kind="opacity" min={30} max={90} options={options} /><Range kind="blur" min={0} max={40} options={options} /><Range kind="dim" min={0} max={60} options={options} /></>, h.sliders)}
    {createPortal(<Range kind="contrast" min={0} max={100} options={options} />, h.contrast)}
    {createPortal(<Switch.Root className="sw" data-ecosystem="" checked={Boolean(options.values.side)} aria-label={options.labels.side} onCheckedChange={value => options.onChange('side', value)} />, h.side)}
    {createPortal(<RadioGroup value={String(options.values.mode)} className="themes" aria-label={options.labels.mode} onValueChange={value => options.onChange('mode', value)} data-ecosystem="">
      {options.choices.mode.map(item => <Radio.Root key={item.value} className="tc" value={item.value}><MiniTheme mode={item.value} /><span className="lab"><i className="radio" />{item.label}{item.value === 'system' && <small>{options.systemTheme}</small>}</span></Radio.Root>)}
    </RadioGroup>, h.themes)}
    {(['fontui', 'fontcode', 'motion', 'lang'] as const).map(kind => createPortal(<Choice kind={kind} options={options} />, h[kind], kind))}
  </>;
}

let root: Root | null = null;
let generation = 0;
export function updateAppearance(options: AppearanceOptions) {
  if (!root) {
    const host = document.getElementById('settings-components-root');
    if (!host) throw new Error('Missing settings component root');
    root = createRoot(host);
  }
  root.render(<Appearance key={generation} options={options} />);
}

export function hideAppearance() { generation++; root?.render(null); }
