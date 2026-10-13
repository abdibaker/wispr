import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

type OverlayState = { state: string; level: number; message: string };

const labels: Record<string, string> = {
  recording: "Listening",
  transcribing: "Transcribing",
  cleaning: "Cleaning up",
  done: "Inserted",
  warning: "Done with warning",
  error: "Error",
};

const BARS = 11;
const CENTER = (BARS - 1) / 2;
const DOTS = 9;
const SPOKES = 8;

export function Overlay() {
  const [current, setCurrent] = useState<OverlayState>({ state: "hidden", level: 0, message: "" });
  // Newest level first; bar i shows the level from |i - CENTER| chunks ago, so speech ripples outward.
  const [history, setHistory] = useState<number[]>(Array(CENTER + 1).fill(0));

  useEffect(() => {
    const unlisten = listen<OverlayState>("overlay", (event) => {
      setCurrent(event.payload);
      if (event.payload.state === "recording") {
        // Speech RMS rarely exceeds 0.3, so scale it up for a lively meter.
        const level = Math.min(1, event.payload.level * 6);
        setHistory((previous) => [level, ...previous.slice(0, CENTER)]);
      } else {
        setHistory(Array(CENTER + 1).fill(0));
      }
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  if (current.state === "hidden") return null;
  const text = current.message || labels[current.state] || "";
  const busy = current.state === "transcribing" || current.state === "cleaning";
  const compact = current.state === "recording" || busy;

  return (
    <div
      className={`pill pill-${current.state}${compact ? " pill-compact" : ""}`}
      role="status"
      aria-live="polite"
      aria-label={compact ? text : undefined}
    >
      {current.state === "recording" ? (
        <span className="meter" aria-hidden="true">
          {Array.from({ length: BARS }, (_, index) => {
            const distance = Math.abs(index - CENTER);
            const level = history[distance] * (1 - distance / (BARS + 2));
            return <span key={index} style={{ transform: `scaleY(${0.25 + level * 0.75})` }} />;
          })}
        </span>
      ) : busy ? (
        <>
          <span className="dots" aria-hidden="true">
            {Array.from({ length: DOTS }, (_, index) => (
              <span key={index} style={{ animationDelay: `${index * 90}ms` }} />
            ))}
          </span>
          <span className="spinner" aria-hidden="true">
            {Array.from({ length: SPOKES }, (_, index) => (
              <span
                key={index}
                style={{ transform: `rotate(${index * (360 / SPOKES)}deg)`, opacity: 0.2 + (index / SPOKES) * 0.8 }}
              />
            ))}
          </span>
        </>
      ) : (
        <>
          <span className="dot" aria-hidden="true" />
          <span className="pill-text" title={text}>
            {text}
          </span>
        </>
      )}
    </div>
  );
}
