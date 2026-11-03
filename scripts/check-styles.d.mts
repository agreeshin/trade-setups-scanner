export declare function findIllegalStyleImports(
  sources: { path: string; content: string }[],
): { path: string; specifier: string; resolved: string }[];
