import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { PairResponder } from "./PairResponder";
import "./windows.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Pair-responder window is missing its #root mount element");
}

createRoot(root).render(
  <StrictMode>
    <PairResponder />
  </StrictMode>,
);
