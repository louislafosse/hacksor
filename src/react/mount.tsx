import { createRoot, type Root } from "react-dom/client";
import type { ReactElement } from "react";

/**
 * Mount a React element into a fresh container on <body>, returning a close
 * function that unmounts and removes it.
 *
 * The migration is incremental: the vanilla UI stays in charge of the shell
 * and hands individual surfaces to React one at a time. Each surface owns its
 * own root, so nothing depends on a single app-wide root existing yet.
 */
export function mountOverlay(render: (close: () => void) => ReactElement): () => void {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const root: Root = createRoot(host);
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    document.removeEventListener("keydown", onKey, true);
    // Unmount on a later tick: React forbids unmounting while it is rendering,
    // and close() is usually called from inside an event handler it owns.
    setTimeout(() => {
      root.unmount();
      host.remove();
    }, 0);
  };
  // Escape closes any overlay mounted this way. Captured, so it fires even
  // while a field inside has focus — an overlay you cannot dismiss from the
  // keyboard is the one you end up clicking around.
  //
  // Only the TOPMOST overlay closes: a dialog opened over another handles the
  // key and stops it, instead of tearing down the whole stack at once.
  function onKey(e: KeyboardEvent) {
    if (e.key !== "Escape" || e.defaultPrevented) return;
    const last = document.body.lastElementChild;
    if (last !== host) return;
    e.preventDefault();
    e.stopPropagation();
    close();
  }
  document.addEventListener("keydown", onKey, true);
  root.render(render(close));
  return close;
}
