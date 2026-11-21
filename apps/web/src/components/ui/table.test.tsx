import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow, TableSkeletonRows } from "./table";

afterEach(cleanup);

describe("Table", () => {
  it("renders semantic table markup with numeric cells", () => {
    render(
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Инструмент</TableHead>
            <TableHead>Сетапов</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          <TableRow>
            <TableCell>BTCUSDT</TableCell>
            <TableCell numeric>12</TableCell>
          </TableRow>
        </TableBody>
      </Table>,
    );
    expect(screen.getByRole("table")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Сетапов" })).toBeInTheDocument();
    expect(screen.getByRole("cell", { name: "12" }).className).toContain("tabular-nums");
    expect(screen.getByText("BTCUSDT")).toBeInTheDocument();
  });

  it("renders exactly the requested placeholder cells without replacing headers or owning loading state", () => {
    const view = render(<Table aria-label="Исследования" aria-busy="true">
      <TableHeader><TableRow><TableHead>ID</TableHead><TableHead>Диапазон дат</TableHead></TableRow></TableHeader>
      <TableBody><TableSkeletonRows rows={20} columns={2} /></TableBody>
    </Table>);
    const table = screen.getByRole("table", { name: "Исследования" });
    expect(table).toHaveAttribute("aria-busy", "true");
    expect(within(table).getAllByRole("columnheader")).toHaveLength(2);
    expect(within(table).getAllByRole("row")).toHaveLength(1);
    expect(table.querySelectorAll("tbody tr")).toHaveLength(20);
    expect(table.querySelectorAll('tbody tr[aria-hidden="true"]')).toHaveLength(20);
    expect(table.querySelectorAll('tbody td [data-slot="skeleton"]')).toHaveLength(40);

    view.rerender(<Table aria-label="Исследования" aria-busy="false">
      <TableHeader><TableRow><TableHead>ID</TableHead><TableHead>Диапазон дат</TableHead></TableRow></TableHeader>
      <TableBody><TableRow><TableCell>#53</TableCell><TableCell>Сохранённый период</TableCell></TableRow></TableBody>
    </Table>);
    expect(within(table).getAllByRole("columnheader")).toHaveLength(2);
    expect(table.querySelector('[data-slot="skeleton"]')).toBeNull();
    expect(within(table).getByRole("cell", { name: "#53" })).toBeInTheDocument();
    expect(table).toHaveAttribute("aria-busy", "false");
  });

  it("allows an explicit zero-row placeholder without inferring loading from an empty table", () => {
    const { container } = render(<Table><TableBody><TableSkeletonRows rows={0} columns={2} /></TableBody></Table>);
    expect(container.querySelectorAll("tbody tr")).toHaveLength(0);
    expect(screen.getByRole("table")).not.toHaveAttribute("aria-busy");
  });

  it.each([
    { rows: -1, columns: 2 }, { rows: 0.5, columns: 2 }, { rows: Infinity, columns: 2 },
    { rows: 501, columns: 2 }, { rows: 2, columns: 0 }, { rows: 2, columns: 1.5 },
    { rows: 2, columns: NaN }, { rows: 2, columns: 101 },
  ])("rejects invalid or unbounded placeholder dimensions: %j", (props) => {
    expect(() => TableSkeletonRows(props)).toThrow(RangeError);
  });
});
