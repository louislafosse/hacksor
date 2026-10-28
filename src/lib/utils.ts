import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** shadcn's class helper: merge conditional classes, last Tailwind rule wins. */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
