import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";

export type VpnStatus = {
  tunnel: boolean; iface: string; ip: string; country: string; isp: string;
  proxy: boolean; hosting: boolean; exposed: boolean; lookup_error: string;
};

/** A labelled switch, built from the same parts as the composer's pill toggle. */
function Toggle({
  checked, onChange, title, children,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <label className="flex cursor-pointer items-start gap-3 py-1.5">
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={cn(
          "relative mt-0.5 h-5 w-9 shrink-0 rounded-full border transition-colors",
          checked ? "border-foreground bg-foreground" : "border-border bg-accent",
        )}
      >
        {/* Positioned with left/top, not transforms: -translate-y-1/2 and
            translate-x-* both write the same property, so they clobbered each
            other — the knob lost its centring and slid oddly between states. */}
        <span
          className={cn(
            "absolute top-1/2 -mt-[7px] size-3.5 rounded-full bg-background shadow-sm",
            "transition-[left] duration-150 ease-out",
            checked ? "left-[18px]" : "left-[2px]",
          )}
        />
      </button>
      <span className="flex min-w-0 flex-col gap-0.5">
        <b className="text-[13.5px] font-medium text-foreground">{title}</b>
        <em className="text-xs not-italic leading-relaxed text-muted-foreground">{children}</em>
      </span>
    </label>
  );
}

export function SecurityTab({
  warnWithoutVpn, onWarnChange,
  limitRuntimeMount, onMountChange,
  checkVpn, describeVpn,
}: {
  warnWithoutVpn: boolean;
  onWarnChange: (v: boolean) => void;
  limitRuntimeMount: boolean;
  onMountChange: (v: boolean) => void;
  checkVpn: () => Promise<VpnStatus>;
  describeVpn: (v: VpnStatus) => string;
}) {
  const [warn, setWarn] = useState(warnWithoutVpn);
  const [narrow, setNarrow] = useState(limitRuntimeMount);
  const [note, setNote] = useState("");
  const [checking, setChecking] = useState(false);

  const check = async () => {
    setChecking(true);
    setNote("Checking…");
    try {
      setNote(describeVpn(await checkVpn()));
    } catch (e) {
      setNote(String(e));
    }
    setChecking(false);
  };

  return (
    <div className="flex flex-col gap-5">
      <section className="flex flex-col gap-2">
        <Label className="text-muted-foreground">Operational safety</Label>
        <Toggle
          checked={warn}
          onChange={(v) => { setWarn(v); onWarnChange(v); }}
          title="Warn before sending in Agent mode without a VPN"
        >
          Checks the default route and, with ip-api.com, how your address looks from
          outside. The check sends your IP to that service.
        </Toggle>
        <div>
          <Button variant="outline" size="sm" onClick={check} disabled={checking}>
            {checking ? "Checking…" : "Check now"}
          </Button>
        </div>
        {note && <p className="text-xs leading-relaxed text-muted-foreground">{note}</p>}
      </section>

      <Separator />

      <section className="flex flex-col gap-2">
        <Label className="text-muted-foreground">Runtime isolation</Label>
        <Toggle
          checked={narrow}
          onChange={(v) => { setNarrow(v); onMountChange(v); }}
          title="Limit the runtime to the working directory"
        >
          The container otherwise mounts your whole home directory. Hacksor's own
          config is still mounted — the agent needs it. Applies next time the
          container starts.
        </Toggle>
      </section>
    </div>
  );
}
