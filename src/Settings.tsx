import { useCallback, useEffect, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type SettingsData = {
  autostart: boolean;
  shortcut: string;
  auto_insert: boolean;
  keep_in_clipboard: boolean;
  notifications: boolean;
  sounds: boolean;
  microphone: string;
  endpoint: string;
  stt_model: string;
  language: string;
  cleanup_enabled: boolean;
  cleanup_model: string;
  reasoning_effort: string;
  cleanup_instructions: string;
  vocabulary: string[];
  history_enabled: boolean;
  history_retention_days: number;
  insertion_method: string;
  debug_logging: boolean;
  stt_timeout_secs: number;
  cleanup_timeout_secs: number;
  min_hold_ms: number;
  max_record_secs: number;
};

type HistoryEntry = {
  id: number;
  created_at: number;
  raw: string;
  cleaned: string | null;
  mode: string;
  stt_model: string;
  cleanup_model: string | null;
  error: string | null;
  duration_ms: number;
  latency_ms: number;
};

type Microphone = { name: string; description: string };
type Diagnostics = Record<string, unknown> & { latency: Record<string, number> | null };

const tabs = ["General", "Speech", "Cleanup", "Vocabulary", "History", "Advanced"] as const;
type Tab = (typeof tabs)[number];

export function Settings() {
  const [tab, setTab] = useState<Tab>("General");
  const [settings, setSettings] = useState<SettingsData | null>(null);
  const [status, setStatus] = useState<{ kind: "ok" | "error"; text: string } | null>(null);

  useEffect(() => {
    invoke<SettingsData>("get_settings").then(setSettings);
  }, []);

  const update = useCallback(
    async (patch: Partial<SettingsData>) => {
      if (!settings) return;
      const next = { ...settings, ...patch };
      setSettings(next);
      try {
        await invoke("save_settings", { settings: next });
        setStatus({ kind: "ok", text: "Saved" });
      } catch (error) {
        setStatus({ kind: "error", text: String(error) });
      }
    },
    [settings],
  );

  useEffect(() => {
    if (status?.kind !== "ok") return;
    const timer = setTimeout(() => setStatus(null), 1500);
    return () => clearTimeout(timer);
  }, [status]);

  if (!settings) return null;

  return (
    <div className="app">
      <nav className="sidebar" aria-label="Settings sections">
        <div className="brand">
          <img src="/icon.svg" alt="" width={28} height={28} />
          <span>Voice Prompt</span>
        </div>
        {tabs.map((name) => (
          <button
            key={name}
            className={name === tab ? "nav active" : "nav"}
            aria-current={name === tab ? "page" : undefined}
            onClick={() => setTab(name)}
          >
            {name}
          </button>
        ))}
        <div className={`status ${status?.kind ?? ""}`} role="status" aria-live="polite">
          {status?.text}
        </div>
      </nav>
      <main className="content">
        {tab === "General" && <General settings={settings} update={update} />}
        {tab === "Speech" && <Speech settings={settings} update={update} />}
        {tab === "Cleanup" && <Cleanup settings={settings} update={update} />}
        {tab === "Vocabulary" && <Vocabulary settings={settings} update={update} />}
        {tab === "History" && <History settings={settings} update={update} />}
        {tab === "Advanced" && <Advanced settings={settings} update={update} />}
      </main>
    </div>
  );
}

type SectionProps = { settings: SettingsData; update: (patch: Partial<SettingsData>) => void };

function Row({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="row">
      <span className="row-label">
        {label}
        {hint ? <small>{hint}</small> : null}
      </span>
      <span className="row-control">{children}</span>
    </label>
  );
}

function Toggle({ checked, onChange }: { checked: boolean; onChange: (value: boolean) => void }) {
  return (
    <input type="checkbox" role="switch" className="switch" checked={checked} onChange={(e) => onChange(e.target.checked)} />
  );
}

/** Text input that saves on blur or Enter, not on every keystroke. */
function Text({
  value,
  onSave,
  ...rest
}: { value: string; onSave: (value: string) => void } & Omit<React.InputHTMLAttributes<HTMLInputElement>, "value" | "onChange">) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  return (
    <input
      {...rest}
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => draft !== value && onSave(draft)}
      onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
    />
  );
}

function NumberInput({ value, onSave, min }: { value: number; onSave: (value: number) => void; min: number }) {
  return (
    <Text
      type="number"
      min={min}
      value={String(value)}
      onSave={(text) => {
        const parsed = Number(text);
        if (Number.isFinite(parsed) && parsed >= min) onSave(Math.round(parsed));
      }}
    />
  );
}

function General({ settings, update }: SectionProps) {
  return (
    <section>
      <h1>General</h1>
      <Row label="Start on login">
        <Toggle checked={settings.autostart} onChange={(autostart) => update({ autostart })} />
      </Row>
      <Row label="Push-to-talk shortcut" hint="Hold to record, release to insert. Examples: Ctrl+Super, Alt+Space, F9.">
        <Text value={settings.shortcut} onSave={(shortcut) => update({ shortcut })} aria-label="Shortcut" />
      </Row>
      <Row label="Insert automatically" hint="Off: copy to the clipboard only.">
        <Toggle checked={settings.auto_insert} onChange={(auto_insert) => update({ auto_insert })} />
      </Row>
      <Row label="Also keep in clipboard" hint="After typing, leave the prompt on the clipboard as a backup.">
        <Toggle checked={settings.keep_in_clipboard} onChange={(keep_in_clipboard) => update({ keep_in_clipboard })} />
      </Row>
      <Row label="Notifications" hint="Shown for errors.">
        <Toggle checked={settings.notifications} onChange={(notifications) => update({ notifications })} />
      </Row>
      <Row label="Sounds" hint="Short tones when recording starts and stops.">
        <Toggle checked={settings.sounds} onChange={(sounds) => update({ sounds })} />
      </Row>
    </section>
  );
}

function Speech({ settings, update }: SectionProps) {
  const [microphones, setMicrophones] = useState<Microphone[]>([]);
  const [hasKey, setHasKey] = useState(false);
  const [key, setKey] = useState("");
  const [test, setTest] = useState<{ ok: boolean; text: string } | null>(null);
  const [testing, setTesting] = useState(false);

  useEffect(() => {
    invoke<Microphone[]>("list_microphones").then(setMicrophones).catch(() => setMicrophones([]));
    invoke<boolean>("has_api_key").then(setHasKey).catch(() => setHasKey(false));
  }, []);

  const saveKey = async () => {
    try {
      await invoke("set_api_key", { key });
      setKey("");
      setHasKey(key.trim().length > 0);
      setTest({ ok: true, text: key.trim() ? "Key saved to the system keyring." : "Key removed." });
    } catch (error) {
      setTest({ ok: false, text: String(error) });
    }
  };

  const runTest = async () => {
    setTesting(true);
    try {
      setTest({ ok: true, text: await invoke<string>("test_connection") });
    } catch (error) {
      setTest({ ok: false, text: String(error) });
    } finally {
      setTesting(false);
    }
  };

  return (
    <section>
      <h1>Speech</h1>
      <Row label="Microphone">
        <select value={settings.microphone} onChange={(e) => update({ microphone: e.target.value })}>
          <option value="">System default</option>
          {microphones.map((mic) => (
            <option key={mic.name} value={mic.name}>
              {mic.description || mic.name}
            </option>
          ))}
        </select>
      </Row>
      <Row label="9Router endpoint" hint="OpenAI-compatible base URL.">
        <Text value={settings.endpoint} onSave={(endpoint) => update({ endpoint })} spellCheck={false} />
      </Row>
      <Row label="API key" hint={hasKey ? "Stored in the system keyring. Enter a new one to replace it." : "Not set."}>
        <span className="inline">
          <input
            type="password"
            autoComplete="off"
            placeholder={hasKey ? "••••••••" : "sk-…"}
            value={key}
            onChange={(e) => setKey(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && saveKey()}
          />
          <button onClick={saveKey} disabled={!key && !hasKey}>
            {key || !hasKey ? "Save" : "Remove"}
          </button>
        </span>
      </Row>
      <Row label="Speech-to-text model">
        <Text value={settings.stt_model} onSave={(stt_model) => update({ stt_model })} spellCheck={false} />
      </Row>
      <Row label="Language" hint="ISO code such as en, or auto.">
        <Text value={settings.language} onSave={(language) => update({ language })} spellCheck={false} />
      </Row>
      <div className="actions">
        <button onClick={runTest} disabled={testing}>
          {testing ? "Testing…" : "Test connection"}
        </button>
        {test ? <span className={test.ok ? "ok" : "error"}>{test.text}</span> : null}
      </div>
    </section>
  );
}

function Cleanup({ settings, update }: SectionProps) {
  return (
    <section>
      <h1>Prompt cleanup</h1>
      <Row label="Clean up prompts" hint="Off: insert the raw transcript.">
        <Toggle checked={settings.cleanup_enabled} onChange={(cleanup_enabled) => update({ cleanup_enabled })} />
      </Row>
      <Row label="Model" hint="Uses the same 9Router endpoint and key.">
        <Text value={settings.cleanup_model} onSave={(cleanup_model) => update({ cleanup_model })} spellCheck={false} />
      </Row>
      <Row label="Reasoning effort">
        <select value={settings.reasoning_effort} onChange={(e) => update({ reasoning_effort: e.target.value })}>
          {["none", "low", "medium", "high"].map((effort) => (
            <option key={effort}>{effort}</option>
          ))}
        </select>
      </Row>
      <Row
        label="Extra instructions"
        hint="Optional. Cleanup is always conservative: it removes fillers and false starts, applies self-corrections, fixes grammar, and never adds requirements."
      >
        <textarea
          rows={4}
          defaultValue={settings.cleanup_instructions}
          placeholder="e.g. Use British spelling."
          onBlur={(e) => e.target.value !== settings.cleanup_instructions && update({ cleanup_instructions: e.target.value })}
        />
      </Row>
    </section>
  );
}

function Vocabulary({ settings, update }: SectionProps) {
  const [term, setTerm] = useState("");
  // Accepts one term or a pasted list (comma- or newline-separated).
  const add = () => {
    const known = new Set(settings.vocabulary.map((w) => w.toLowerCase()));
    const added = term
      .split(/[,\n]/)
      .map((w) => w.trim())
      .filter((w) => w && !known.has(w.toLowerCase()) && known.add(w.toLowerCase()));
    if (added.length) update({ vocabulary: [...settings.vocabulary, ...added] });
    setTerm("");
  };
  return (
    <section>
      <h1>Vocabulary</h1>
      <p className="lead">
        Terms passed to speech recognition as hints and used to correct spelling during cleanup. Add the names you
        dictate often: projects, repositories, files, packages, commands, models and identifiers. Paste a comma- or
        newline-separated list to add several. Speech recognition sees only the first ~400 characters, so put the most
        important terms first; cleanup sees them all.
      </p>
      <div className="inline">
        <input
          value={term}
          placeholder="Add terms, e.g. Cargo.toml, pnpm check, whisper-large-v3"
          aria-label="New vocabulary term"
          onChange={(e) => setTerm(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && add()}
        />
        <button onClick={add}>Add</button>
      </div>
      <ul className="chips">
        {settings.vocabulary.map((word) => (
          <li key={word}>
            {word}
            <button
              aria-label={`Remove ${word}`}
              onClick={() => update({ vocabulary: settings.vocabulary.filter((w) => w !== word) })}
            >
              ×
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

function History({ settings, update }: SectionProps) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [message, setMessage] = useState("");
  const load = useCallback(() => {
    invoke<HistoryEntry[]>("history_list").then(setEntries);
  }, []);

  useEffect(() => {
    load();
    const unlisten = listen("history-changed", load);
    return () => {
      unlisten.then((stop) => stop());
    };
  }, [load]);

  const copy = async (text: string) => {
    await invoke("copy_text", { text });
    setMessage("Copied to clipboard");
    setTimeout(() => setMessage(""), 1500);
  };

  return (
    <section>
      <h1>History</h1>
      <Row label="Keep history" hint="Stored only on this computer. Audio is never stored.">
        <Toggle checked={settings.history_enabled} onChange={(history_enabled) => update({ history_enabled })} />
      </Row>
      <Row label="Retention (days)" hint="0 keeps everything.">
        <NumberInput
          min={0}
          value={settings.history_retention_days}
          onSave={(history_retention_days) => update({ history_retention_days })}
        />
      </Row>
      <div className="actions">
        <button
          className="danger"
          disabled={entries.length === 0}
          onClick={async () => {
            if (!window.confirm("Delete all history?")) return;
            await invoke("history_clear");
            load();
          }}
        >
          Clear history
        </button>
        <span className="ok">{message}</span>
      </div>
      {entries.length === 0 ? <p className="lead">No dictations yet.</p> : null}
      <ul className="history">
        {entries.map((entry) => (
          <li key={entry.id}>
            <div className="meta">
              {new Date(entry.created_at).toLocaleString()} · {entry.mode} · {(entry.duration_ms / 1000).toFixed(1)}s audio ·{" "}
              {(entry.latency_ms / 1000).toFixed(1)}s
              {entry.error ? <span className="error"> · cleanup failed</span> : null}
            </div>
            <p className="prompt">{entry.cleaned ?? entry.raw}</p>
            {entry.cleaned ? (
              <details>
                <summary>Raw transcript</summary>
                <p>{entry.raw}</p>
              </details>
            ) : null}
            <div className="inline">
              <button onClick={() => copy(entry.cleaned ?? entry.raw)}>Copy</button>
              {entry.cleaned ? <button onClick={() => copy(entry.raw)}>Copy raw</button> : null}
              <button
                className="danger"
                onClick={async () => {
                  await invoke("history_delete", { id: entry.id });
                  load();
                }}
              >
                Delete
              </button>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}

function Advanced({ settings, update }: SectionProps) {
  const [diagnostics, setDiagnostics] = useState<Diagnostics | null>(null);
  const refresh = () => invoke<Diagnostics>("diagnostics").then(setDiagnostics);
  useEffect(() => {
    refresh();
  }, []);

  return (
    <section>
      <h1>Advanced</h1>
      <Row label="Insertion method" hint="Paste methods overwrite the clipboard.">
        <select value={settings.insertion_method} onChange={(e) => update({ insertion_method: e.target.value })}>
          <option value="type">Type (virtual keyboard)</option>
          <option value="paste">Paste with Ctrl+V</option>
          <option value="paste-terminal">Paste with Ctrl+Shift+V (terminals)</option>
          <option value="clipboard">Clipboard only</option>
        </select>
      </Row>
      <Row label="Debug logging" hint="Adds timing details. Prompt text is never logged.">
        <Toggle checked={settings.debug_logging} onChange={(debug_logging) => update({ debug_logging })} />
      </Row>
      <Row label="Transcription timeout (s)">
        <NumberInput min={5} value={settings.stt_timeout_secs} onSave={(stt_timeout_secs) => update({ stt_timeout_secs })} />
      </Row>
      <Row label="Cleanup timeout (s)" hint="On timeout the raw transcript is inserted.">
        <NumberInput
          min={3}
          value={settings.cleanup_timeout_secs}
          onSave={(cleanup_timeout_secs) => update({ cleanup_timeout_secs })}
        />
      </Row>
      <Row label="Minimum hold (ms)" hint="Shorter taps are ignored.">
        <NumberInput min={0} value={settings.min_hold_ms} onSave={(min_hold_ms) => update({ min_hold_ms })} />
      </Row>
      <Row label="Maximum recording (s)">
        <NumberInput min={10} value={settings.max_record_secs} onSave={(max_record_secs) => update({ max_record_secs })} />
      </Row>
      <h2>
        Diagnostics <button onClick={refresh}>Refresh</button>
      </h2>
      {diagnostics ? (
        <dl className="diagnostics">
          {Object.entries(diagnostics)
            .filter(([name]) => name !== "latency")
            .map(([name, value]) => (
              <div key={name}>
                <dt>{name.replaceAll("_", " ")}</dt>
                <dd>{String(value)}</dd>
              </div>
            ))}
          {diagnostics.latency
            ? Object.entries(diagnostics.latency).map(([name, value]) => (
                <div key={name}>
                  <dt>{name.replaceAll("_", " ")}</dt>
                  <dd>{value} ms</dd>
                </div>
              ))
            : null}
        </dl>
      ) : null}
    </section>
  );
}
