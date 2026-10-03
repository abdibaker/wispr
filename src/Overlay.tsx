import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

type OverlayState = { state: string; level: number; message: string };

const labels: Record<string, string> = {
  recording: "Listening",
  transcribing: "Transcribing…",
  cleaning: "Cleaning up…",
  done: "Inserted",
  warning: "Done with warning",
  error: "Error",
};

const BARS = 7;

export function Overlay() {
  const [current, setCurrent] = useState<OverlayState>({ state: "hidden", level: 0, message: "" });
  const [levels, setLevels] = useState<number[]>(Array(BARS).fill(0));

  useEffect(() => {
    const unlisten = listen<OverlayState>("overlay", (event) => {
      setCurrent(event.payload);
      if (event.payload.state === "recording") {
        // Speech RMS rarely exceeds 0.3, so scale it up for a lively meter.
        const level = Math.min(1, event.payload.level * 6);
        setLevels((previous) => [...previous.slice(1), level]);
      } else {
        setLevels(Array(BARS).fill(0));
      }
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  if (current.state === "hidden") return null;
  const text = current.message || labels[current.state] || "";

  return (
    <div className={`pill pill-${current.state}`} role="status" aria-live="polite">
      <span className="dot" />
      {current.state === "recording" ? (
        <span className="meter" aria-label="Microphone level">
          {levels.map((level, index) => (
            <span key={index} style={{ transform: `scaleY(${0.15 + level * 0.85})` }} />
          ))}
        </span>
      ) : null}
      <span className="pill-text" title={text}>
        {text}
      </span>
    </div>
  );
}
