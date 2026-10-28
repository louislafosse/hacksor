import { useState } from "react";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import { EmojiPicker, QUICK } from "@/react/EmojiPicker";

export const CAT_COLORS = ["#ff4500", "#f59e0b", "#51da4c", "#38bdf8", "#a78bfa", "#f472b6", "#94a3b8"];
export const CAT_EMOJI = QUICK;

export function CategoryDialog({
  onCreate,
  onClose,
}: {
  onCreate: (name: string, emoji: string, color: string) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState("");
  const [emoji, setEmoji] = useState(CAT_EMOJI[0]);
  const [color, setColor] = useState(CAT_COLORS[0]);

  const create = () => {
    const n = name.trim();
    if (!n) return;
    onCreate(n, emoji, color);
    onClose();
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-[420px]">
        <DialogHeader>
          <DialogTitle>New category</DialogTitle>
          <DialogDescription>
            Group chats in the sidebar. Drag a chat onto the header to file it.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          <div className="grid gap-2">
            <Label htmlFor="cat-name">Name</Label>
            <Input
              id="cat-name"
              autoFocus
              maxLength={40}
              placeholder="Scope, client, phase …"
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); create(); } }}
            />
          </div>

          <div className="grid gap-2">
            <div className="flex items-center justify-between">
              <Label>Icon</Label>
              <span className="grid size-7 place-content-center rounded-md border text-[17px] leading-none">{emoji}</span>
            </div>
            <EmojiPicker value={emoji} onPick={setEmoji} />
          </div>

          <div className="grid gap-2">
            <Label>Colour</Label>
            <div className="flex flex-wrap gap-2">
              {CAT_COLORS.map((c) => (
                <button
                  key={c}
                  type="button"
                  aria-label={c}
                  onClick={() => setColor(c)}
                  style={{ background: c }}
                  className={cn(
                    "size-7 rounded-full border transition-shadow",
                    color === c
                      ? "border-foreground ring-2 ring-foreground ring-offset-2 ring-offset-background"
                      : "border-border hover:border-ring",
                  )}
                />
              ))}
            </div>
          </div>
        </div>

        <DialogFooter className="mt-1 border-t pt-4">
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button onClick={create} disabled={!name.trim()}>Create</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
