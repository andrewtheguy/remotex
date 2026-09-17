import assert from "node:assert/strict";
import { test } from "node:test";

import {
  plotX,
  pointedSecond,
  runs,
  SCALE_WIDTH,
  tipLeft,
} from "./usageChart.ts";

test("the read seconds are drawn in runs, broken by each gap", () => {
  assert.deepEqual(runs([1, 2, 3]), [[0, 3]]);
  assert.deepEqual(runs([null, 1, 2, null, null, 3, null]), [
    [1, 3],
    [5, 6],
  ]);
  assert.deepEqual(runs([null, null]), []);
  assert.deepEqual(runs([]), []);
  assert.deepEqual(runs([0]), [[0, 1]], "a second nothing moved in is read");
});

test("the seconds span the plot, which stops short of the scale", () => {
  const width = 100 + SCALE_WIDTH;
  assert.equal(plotX(0, 60, width), 0);
  assert.equal(plotX(59, 60, width), 100);
  assert.equal(pointedSecond(0, 60, width), 0);
  assert.equal(pointedSecond(100, 60, width), 59);
  assert.equal(pointedSecond(50, 60, width), 30);
  assert.equal(pointedSecond(-1, 60, width), null);
  assert.equal(pointedSecond(101, 60, width), null, "over the scale");
});

test("the tooltip sits over its second but inside the plot", () => {
  const width = 400 + SCALE_WIDTH;
  assert.equal(tipLeft(0, 60, width), 60);
  assert.equal(tipLeft(59, 60, width), 340);
  assert.equal(tipLeft(30, 61, width), 200);
});
