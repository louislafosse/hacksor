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
  const close = () => {
    // Unmount on a later tick: React forbids unmounting while it is rendering,
    // and close() is usually called from inside an event handler it owns.
    setTimeout(() => {
      root.unmount();
      host.remove();
    }, 0);
  };
  root.render(render(close));
  return close;
}
