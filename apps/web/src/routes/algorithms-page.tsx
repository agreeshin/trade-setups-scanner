import { PageHeader } from "../components/ui/page-header";
import { AlgorithmSettings } from "./algorithm-settings";

export function AlgorithmsPage() {
  return <section aria-labelledby="algorithms-page-heading" className="grid gap-6">
    <PageHeader id="algorithms-page-heading" title="Алгоритмы" />
    <AlgorithmSettings />
  </section>;
}
