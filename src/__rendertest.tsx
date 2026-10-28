import { createRoot } from "react-dom/client";
import { createElement } from "react";
import { CategoryDialog } from "@/react/CategoryDialog";
import "./styles.css";

const host = document.getElementById("root")!;
// type "bird" into the picker so the capture shows real search results
setTimeout(() => {
  const inp = [...document.querySelectorAll("input")].find((i) => (i.placeholder || "").includes("Search emoji"));
  if (!inp) return;
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
  setter.call(inp, "bird");
  inp.dispatchEvent(new Event("input", { bubbles: true }));
}, 900);

createRoot(host).render(
  createElement("div", { className: "dark" },
    createElement(CategoryDialog, { onCreate: () => {}, onClose: () => {} }),
  ),
);
