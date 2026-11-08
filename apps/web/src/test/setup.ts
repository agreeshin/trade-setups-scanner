import "@testing-library/jest-dom/vitest";

class ResizeObserverStub {
  observe = (target: Element): void => {
    void target;
  };
  unobserve = (target: Element): void => {
    void target;
  };
  disconnect = (): undefined => undefined;
}
if (!("ResizeObserver" in globalThis)) {
  globalThis.ResizeObserver = ResizeObserverStub;
}
