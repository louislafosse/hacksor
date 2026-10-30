import { Gauge, ShieldCheck, Target, Check } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSeparator,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

export type Choice = { v: string; label: string };

/** What the chat's `/goal` is doing, for the menu row that controls it. */
export type GoalSummary = { text: string; running: boolean; round: string };

/**
 * The composer's overflow menu: reasoning effort, approval mode and the goal.
 *
 * Replaces a hand-rolled popover whose rows were only clickable on the small
 * chip at their right edge — clicking the icon or the word "Effort" did
 * nothing, which read as the menu being broken. Radix makes the whole row the
 * target and handles focus, keyboard and dismissal, so that class of bug
 * cannot come back.
 */
export function ComposerMenu({
  efforts,
  effort,
  onEffort,
  perms,
  permission,
  onPermission,
  effortDisabled,
  permDisabled,
  goal,
  onGoalNew,
  onGoalStop,
  onGoalResume,
}: {
  efforts: Choice[];
  effort: string;
  onEffort: (v: string) => void;
  perms: Choice[];
  permission: string;
  onPermission: (v: string) => void;
  effortDisabled?: boolean;
  /** Ask mode runs nothing, so approval has nothing to approve. */
  permDisabled?: boolean;
  /** Undefined when this chat has never been given a goal. */
  goal?: GoalSummary;
  onGoalNew: () => void;
  onGoalStop: () => void;
  onGoalResume: () => void;
}) {
  const label = (list: Choice[], v: string) => list.find((c) => c.v === v)?.label ?? "";

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        className="rounded-lg px-2.5 py-1.5 text-muted-foreground hover:bg-accent hover:text-foreground data-[state=open]:bg-accent data-[state=open]:text-foreground"
        title="Effort, access and the chat’s goal"
        aria-label="More composer options"
      >
        <span className="text-[15px] leading-none tracking-widest">⋯</span>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="start" side="top" className="min-w-52">
        {!effortDisabled && (
          <DropdownMenuSub>
            <DropdownMenuSubTrigger className="gap-2">
              <Gauge className="size-4 opacity-80" />
              <span className="flex-1">Effort</span>
              <span className="text-muted-foreground">{label(efforts, effort)}</span>
            </DropdownMenuSubTrigger>
            <DropdownMenuSubContent>
              <DropdownMenuRadioGroup value={effort} onValueChange={onEffort}>
                {efforts.map((e) => (
                  <DropdownMenuRadioItem key={e.v} value={e.v}>{e.label}</DropdownMenuRadioItem>
                ))}
              </DropdownMenuRadioGroup>
            </DropdownMenuSubContent>
          </DropdownMenuSub>
        )}

        {!permDisabled && (
        <DropdownMenuSub>
          <DropdownMenuSubTrigger className="gap-2">
            <ShieldCheck className="size-4 opacity-80" />
            <span className="flex-1">Access</span>
            <span className="text-muted-foreground">{label(perms, permission)}</span>
          </DropdownMenuSubTrigger>
          <DropdownMenuSubContent>
            <DropdownMenuRadioGroup value={permission} onValueChange={onPermission}>
              {perms.map((p) => (
                <DropdownMenuRadioItem key={p.v} value={p.v}>{p.label}</DropdownMenuRadioItem>
              ))}
            </DropdownMenuRadioGroup>
          </DropdownMenuSubContent>
        </DropdownMenuSub>
        )}

        {(!effortDisabled || !permDisabled) && <DropdownMenuSeparator />}

        {/* The goal is the one thing here that is per-chat rather than a
            setting, so it gets plain items: the menu shows what it is doing
            and the single action that applies right now. */}
        {goal?.running ? (
          <DropdownMenuItem className="gap-2" onSelect={onGoalStop}>
            <Target className="size-4 opacity-80" />
            <span className="flex-1">Stop goal</span>
            <span className="text-muted-foreground">{goal.round}</span>
          </DropdownMenuItem>
        ) : (
          <>
            {goal && (
              <DropdownMenuItem className="gap-2" onSelect={onGoalResume}>
                <Target className="size-4 opacity-80" />
                <span className="flex-1">Resume goal</span>
                <span className="text-muted-foreground">{goal.round}</span>
              </DropdownMenuItem>
            )}
            <DropdownMenuItem className="gap-2" onSelect={onGoalNew}>
              <Target className="size-4 opacity-80" />
              <span className="flex-1">{goal ? "New goal…" : "Set a goal…"}</span>
              <span className="text-muted-foreground">/goal</span>
            </DropdownMenuItem>
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

export const _unused = Check;
