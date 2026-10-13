import { useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { flushSync } from 'react-dom';
import { Combobox } from '@base-ui/react/combobox';

export interface ModelRow {
  value: string;
  label: string;
  group: string;
  description?: string;
}

export interface ModelPickerOptions {
  anchor: HTMLElement;
  rows: ModelRow[];
  selected: string;
  label: string;
  placeholder: string;
  empty: string;
  current?: { label: string; description: string };
  hint?: string;
  allowFreeform: boolean;
  onSelect: (value: string) => void;
  onClose: () => void;
}

function ModelPicker({ host, options }: { host: HTMLElement; options: ModelPickerOptions }) {
  const [query, setQuery] = useState('');
  const input = useRef<HTMLInputElement>(null);
  const groups = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const rows = options.rows.filter(row => !needle || `${row.label} ${row.description ?? ''}`.toLowerCase().includes(needle));
    // A typed selector remains an explicit first choice, including when saved items match.
    if (options.allowFreeform && query.trim() && !rows.some(row => row.value === query.trim())) rows.unshift({ value: query.trim(), label: query.trim(), group: '' });
    const grouped = new Map<string, ModelRow[]>();
    rows.forEach(row => grouped.set(row.group, [...(grouped.get(row.group) ?? []), row]));
    return [...grouped].map(([label, items]) => ({ label, items }));
  }, [options.rows, options.allowFreeform, query]);
  useEffect(() => {
    input.current?.focus();
    return () => {
      if (options.anchor.isConnected && (host.contains(document.activeElement) || document.activeElement === document.body)) options.anchor.focus();
    };
  }, [host, options.anchor]);
  return <div data-ecosystem="">
    {options.current && <div className="mi cur"><span className="mt mono"><span>{options.current.label}</span><small>{options.current.description}</small></span></div>}
    {options.hint && <div className="hint">{options.hint}</div>}
    <Combobox.Root<ModelRow> inline open items={groups} filter={null} inputValue={query} onInputValueChange={setQuery} autoHighlight value={options.rows.find(row => row.value === options.selected) ?? null} itemToStringLabel={row => row.label} isItemEqualToValue={(a, b) => a.value === b.value} onValueChange={row => { if (row) options.onSelect(row.value); }} onOpenChange={open => { if (!open) options.onClose(); }}>
      <div className="field"><Combobox.Input ref={input} aria-label={options.label} placeholder={options.placeholder} spellCheck={false} autoComplete="off" /></div>
      <Combobox.List className="mp-list scroll">
        {(group: { label: string; items: ModelRow[] }) => <Combobox.Group key={group.label} items={group.items}>
          {group.label && <Combobox.GroupLabel className="lbl">{group.label}</Combobox.GroupLabel>}
          <Combobox.Collection>{(row: ModelRow) => <Combobox.Item key={row.value} value={row} className="mi">
            <Combobox.ItemIndicator><svg className="i sm ck" aria-hidden="true"><use href="#i-check" /></svg></Combobox.ItemIndicator>
            <span className="mt mono"><span>{row.label}</span>{row.description && <small>{row.description}</small>}</span>
          </Combobox.Item>}</Combobox.Collection>
        </Combobox.Group>}
      </Combobox.List>
      <Combobox.Empty className="hint">{options.empty}</Combobox.Empty>
    </Combobox.Root>
  </div>;
}

export function mountModelPicker(host: HTMLElement, options: ModelPickerOptions) {
  const root = createRoot(host);
  flushSync(() => root.render(<ModelPicker host={host} options={options} />));
  return () => root.unmount();
}
