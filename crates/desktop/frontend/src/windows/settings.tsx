import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { SettingsPanel } from "./SettingsPanel";
import "./settings.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Settings window is missing its #root mount element");
}

createRoot(root).render(
  <StrictMode>
    <SettingsPanel />
  </StrictMode>,
);
