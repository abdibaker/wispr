import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Overlay } from "./Overlay";
import { Settings } from "./Settings";
import "./styles.css";

const isOverlay = window.location.hash === "#overlay";
document.documentElement.classList.toggle("overlay-root", isOverlay);

createRoot(document.getElementById("root")!).render(
  <StrictMode>{isOverlay ? <Overlay /> : <Settings />}</StrictMode>,
);
