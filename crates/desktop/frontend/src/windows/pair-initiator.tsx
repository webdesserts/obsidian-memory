import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { PairInitiator } from "./PairInitiator";
import "./windows.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Pair-initiator window is missing its #root mount element");
}

createRoot(root).render(
  <StrictMode>
    <PairInitiator />
  </StrictMode>,
);
