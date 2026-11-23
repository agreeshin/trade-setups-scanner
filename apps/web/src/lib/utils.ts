import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";
/** HTTP-safe 128-bit request identity, including non-secure HTTP origins. */
export function randomRequestId(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
