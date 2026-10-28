import { createRoot } from "react-dom/client";
import { createElement } from "react";
import { SecurityTab } from "@/react/settings/SecurityTab";
import "./styles.css";

const host = document.getElementById("root")!;
createRoot(host).render(
  createElement("div", { className: "dark" }, [
    createElement(SecurityTab, {
      key: "sec-on",
      warnWithoutVpn: true,
      onWarnChange: () => {},
      limitRuntimeMount: false,
      onMountChange: () => {},
      checkVpn: async () => ({ tunnel: false, iface: "en0", ip: "1.2.3.4", country: "X", isp: "Y", proxy: false, hosting: false, exposed: true, lookup_error: "" }),
      describeVpn: () => "Exposed — 1.2.3.4 via en0.",
    }),
    createElement(SecurityTab, {
      key: "sec-off",
      warnWithoutVpn: false,
      onWarnChange: () => {},
      limitRuntimeMount: true,
      onMountChange: () => {},
      checkVpn: async () => ({ tunnel: true, iface: "utun3", ip: "", country: "", isp: "", proxy: false, hosting: false, exposed: false, lookup_error: "" }),
      describeVpn: () => "Protected via utun3.",
    }),
  ]),
);
