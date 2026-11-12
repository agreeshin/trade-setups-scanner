import { AlgorithmSettings } from "./algorithm-settings";

export function AlgorithmsPage() {
  return <section aria-labelledby="algorithms-page-heading" className="grid gap-6">
    <h1 id="algorithms-page-heading" className="sr-only">Алгоритмы</h1>
    <AlgorithmSettings />
  </section>;
}
