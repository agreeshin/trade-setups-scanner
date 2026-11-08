import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "./table";

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
});
