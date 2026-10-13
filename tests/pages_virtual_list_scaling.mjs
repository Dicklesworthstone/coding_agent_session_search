/**
 * Production virtual-list arithmetic/lifecycle tests, with a minimal DOM.
 * Run: node --test tests/pages_virtual_list_scaling.mjs
 * No browser, network, dependencies, or archive contents are required.
 */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { afterEach, beforeEach, test } from "node:test";

const source = await readFile(
  process.env.VIRTUAL_LIST_MODULE || new URL("../src/pages_assets/virtual-list.js", import.meta.url),
  "utf8",
);
const { VariableHeightVirtualList } = await import(
  `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
);

class Element {
  style = {};
  dataset = {};
  children = [];
  parent = null;
  root = false;
  clientHeight = 300;
  clientWidth = 800;
  offsetHeight = 100;
  scrollTop = 0;
  listeners = new Map();
  get isConnected() { return this.root || Boolean(this.parent?.isConnected); }
  set innerHTML(value) {
    assert.equal(value, "");
    for (const child of [...this.children]) child.remove();
  }
  appendChild(element) {
    element.remove();
    element.parent = this;
    this.children.push(element);
    return element;
  }
  remove() {
    if (this.parent) {
      this.parent.children = this.parent.children.filter((child) => child !== this);
      this.parent = null;
    }
  }
  addEventListener(name, callback) { this.listeners.set(name, callback); }
  removeEventListener(name, callback) {
    if (this.listeners.get(name) === callback) this.listeners.delete(name);
  }
}

class Observer {
  static instances = [];
  observed = new Set();
  constructor(callback) {
    this.callback = callback;
    Observer.instances.push(this);
  }
  observe(element) { this.observed.add(element); }
  unobserve(element) { this.observed.delete(element); }
  disconnect() { this.observed.clear(); }
  notify(elements = [...this.observed]) {
    this.callback(elements.map((target) => ({ target })), this);
  }
}

let frames;
let nextFrame;
let originals;
let lists;
beforeEach(() => {
  frames = new Map();
  nextFrame = 0;
  lists = [];
  Observer.instances = [];
  originals = {
    document: globalThis.document,
    ResizeObserver: globalThis.ResizeObserver,
    requestAnimationFrame: globalThis.requestAnimationFrame,
    cancelAnimationFrame: globalThis.cancelAnimationFrame,
    debug: console.debug,
  };
  globalThis.document = { createElement: () => new Element() };
  globalThis.ResizeObserver = Observer;
  globalThis.requestAnimationFrame = (callback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  };
  globalThis.cancelAnimationFrame = (id) => frames.delete(id);
  console.debug = () => {};
});
afterEach(() => {
  for (const list of lists) list.destroy();
  for (const [key, value] of Object.entries(originals)) {
    if (key === "debug") console.debug = value;
    else if (value === undefined) delete globalThis[key];
    else globalThis[key] = value;
  }
});

function flushFrames() {
  let passes = 0;
  while (frames.size) {
    assert.ok(++passes < 100, "animation frames must converge");
    const callbacks = [...frames.values()];
    frames.clear();
    for (const callback of callbacks) callback();
  }
}

function listWith(count, options = {}) {
  const container = new Element();
  container.root = true;
  const list = new VariableHeightVirtualList({
    container,
    totalCount: count,
    estimatedItemHeight: 100,
    overscan: 1,
    renderItem: () => new Element(),
    ...options,
  });
  lists.push(list);
  return list;
}

function heightIndex(count, estimate = 100) {
  const list = Object.create(VariableHeightVirtualList.prototype);
  Object.assign(list, {
    totalCount: count,
    estimatedHeight: estimate,
    heights: new Map(),
    _heightTree: new Map(),
  });
  return list;
}

test("one measurement does not scan the rest of a 100,000-message transcript", () => {
  const list = listWith(100_000);
  let reads = 0;
  const get = list.heights.get.bind(list.heights);
  list.heights.get = (key) => { reads += 1; return get(key); };
  const element = list.items.get(0);
  element.offsetHeight = 175;
  list._measureItem(0, element);
  flushFrames();
  assert.ok(reads < 64, `measurement read ${reads} row heights`);
  assert.equal(list._getItemPosition(99_999), 9_999_975);
});

test("opening a ten-million-row index allocates only mounted/observed height state", () => {
  const list = listWith(10_000_000);
  assert.ok(list.items.size <= 6);
  assert.equal(list._heightTree.size, 0);
  flushFrames();
  assert.ok(list.heights.size <= 6);
  assert.equal(list._heightTree.size, 0, "default-height rows need no tree entries");
  list.scrollToIndex(9_999_900);
  flushFrames();
  assert.ok(list.items.size <= 7);
  assert.ok(list.heights.size <= 13);
  assert.equal(list._getItemPosition(9_999_900), 999_990_000);
});

test("sparse prefix sums and offset lookup agree with a dense oracle after random edits", () => {
  let seed = 1234567;
  const random = () => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed;
  };
  for (const count of [1, 2, 3, 7, 16, 31, 127, 513]) {
    const list = heightIndex(count, 75.5);
    const heights = Array(count).fill(75.5);
    for (let round = 0; round < 100; round += 1) {
      const index = random() % count;
      const height = (random() % 2000 + 1) / 2;
      list._setItemHeight(index, height);
      heights[index] = height;
      let prefix = 0;
      for (let i = 0; i < count; i += 1) {
        assert.equal(list._getItemPosition(i), prefix);
        assert.equal(list._findIndexAtPosition(prefix), i);
        assert.equal(list._findIndexAtPosition(prefix + heights[i] / 2), i);
        prefix += heights[i];
      }
      assert.equal(list._getItemPosition(count), prefix);
      assert.equal(list._findIndexAtPosition(prefix + 1000), count - 1);
    }
  }
});

test("signed-index boundary stays sparse and lookups perform at most 31 tree reads", () => {
  const count = 0x7fffffff;
  const list = heightIndex(count);
  list._setItemHeight(0, 120);
  list._setItemHeight(count - 1, 90);
  assert.ok(list._heightTree.size <= 62);
  let reads = 0;
  const get = list._heightTree.get.bind(list._heightTree);
  list._heightTree.get = (key) => { reads += 1; return get(key); };
  assert.equal(list._findIndexAtPosition((count - 1) * 100 + 20), count - 1);
  assert.ok(reads <= 31, `lookup made ${reads} reads`);
  assert.equal(list._getItemPosition(count), count * 100 + 10);
});

test("shrinking clamps scroll and removes heights which must not reappear on growth", () => {
  const list = listWith(100);
  flushFrames();
  list.scrollToIndex(90);
  flushFrames();
  const row = list.items.get(90);
  row.offsetHeight = 900;
  list._measureItem(90, row);
  flushFrames();
  list.updateTotalCount(5);
  assert.equal(list.scrollTop, 200);
  assert.ok([...list.items.keys()].every((index) => index < 5));
  assert.ok([...list.heights.keys()].every((index) => index < 5));
  list.updateTotalCount(100);
  assert.equal(list._getItemHeight(90), 100);
  assert.equal(list._getItemPosition(100), 10_000);
  list.updateTotalCount(0);
  assert.equal(list.items.size, 0);
  assert.equal(list.scrollTop, 0);
  assert.equal(list._getItemPosition(0), 0);
  list.scrollToIndex(0);
  flushFrames();
  assert.equal(list.items.size, 0);
});

test("count growth rebuilds only measured rows and extends their prefix corrections", () => {
  const list = heightIndex(8);
  list._setItemHeight(0, 125);
  list._setItemHeight(7, 50);
  list.totalCount = 1_000_003;
  list._calculatePositions();
  assert.equal(list._getItemPosition(8), 775);
  assert.equal(list._getItemPosition(list.totalCount), list.totalCount * 100 - 25);
  assert.ok(list._heightTree.size < 50);
});

test("invalid dimensions/count updates fail before changing list state", () => {
  for (const totalCount of [-1, 1.5, NaN, Infinity, 0x80000000]) {
    assert.throws(() => listWith(totalCount), RangeError);
  }
  for (const estimatedItemHeight of [0, -1, NaN, Infinity, Number.MAX_VALUE]) {
    assert.throws(() => listWith(10, { estimatedItemHeight }), RangeError);
  }
  assert.throws(() => listWith(10, { overscan: -1 }), RangeError);
  const list = listWith(10);
  const count = list.totalCount;
  assert.throws(() => list.updateTotalCount(-1), RangeError);
  assert.equal(list.totalCount, count);
  for (const index of [-1, 0.5, NaN, Infinity, 10]) list.scrollToIndex(index);
  assert.equal(list.scrollTop, 0);
});

test("zero/invalid and stale recycled measurements do not corrupt positions", () => {
  const list = listWith(100);
  flushFrames();
  const oldRow = list.items.get(0);
  for (const height of [0, -1, NaN, Infinity]) {
    oldRow.offsetHeight = height;
    list._measureItem(0, oldRow);
  }
  assert.equal(list._getItemPosition(1), 100);
  list.refresh();
  // Reconnecting the old element elsewhere must not make it authoritative.
  list.container.appendChild(oldRow);
  oldRow.offsetHeight = 999;
  list._measureItem(0, oldRow);
  assert.equal(list._getItemPosition(1), 100);
  flushFrames();
  list.destroy();
  list.destroy();
  list.refresh();
  list.updateTotalCount(10);
  list.scrollToIndex(0);
  assert.equal(list.items.size, 0);
  assert.equal(list.heights.size, 0);
  assert.equal(list._heightTree.size, 0);
});

test("start, center and end navigation use the measured variable row height", () => {
  const list = listWith(100);
  flushFrames();
  list._setItemHeight(50, 200);
  list._updateTotalHeight();
  list.scrollToIndex(50, "start");
  assert.equal(list.scrollTop, 5000);
  list.scrollToIndex(50, "center");
  assert.equal(list.scrollTop, 4950);
  list.scrollToIndex(50, "end");
  assert.equal(list.scrollTop, 4900);
});

function notifyRows(list, indices) {
  const rows = indices.map((index) => list.items.get(index));
  for (const observer of Observer.instances) {
    const observed = rows.filter((row) => observer.observed.has(row));
    if (observed.length) observer.notify(observed);
  }
}

test("post-render expansion and collapse update subsequent message positions", () => {
  const list = listWith(100);
  flushFrames();
  const first = list.items.get(0);
  first.offsetHeight = 400;
  notifyRows(list, [0]);
  flushFrames();
  assert.equal(list._getItemPosition(1), 400);
  assert.equal(list.items.get(1).style.top, "400px");
  assert.equal(list.scrollTop, 0);
  first.offsetHeight = 50;
  notifyRows(list, [0]);
  flushFrames();
  assert.equal(list.items.get(1).style.top, "50px");
  assert.equal(list._getItemPosition(100), 9950);
});

test("resize notifications coalesce into one measurement frame and one layout write phase", () => {
  const list = listWith(100);
  flushFrames();
  const events = [];
  const rows = [list.items.get(0), list.items.get(1)];
  rows.forEach((row, index) => {
    Object.defineProperty(row, "offsetHeight", {
      get() { events.push(`read${index}`); return 130 + index * 20; },
    });
    Object.defineProperty(row.style, "top", {
      set() { events.push(`write${index}`); },
    });
  });
  let commits = 0;
  const commit = list._commitLayout.bind(list);
  list._commitLayout = (anchor) => { commits += 1; commit(anchor); };
  for (let n = 0; n < 20; n += 1) notifyRows(list, [0, 1]);
  assert.equal(frames.size, 1);
  assert.deepEqual(events, []);
  flushFrames();
  assert.equal(commits, 1);
  assert.deepEqual(events.slice(0, 2), ["read0", "read1"]);
  assert.equal(list._getItemPosition(2), 280);
});

test("an overscan row growing above the viewport preserves the visible row and offset", () => {
  const list = listWith(100);
  flushFrames();
  list.scrollToIndex(20);
  list.container.scrollTop += 25;
  list._onScroll();
  flushFrames();
  const above = list.items.get(19);
  above.offsetHeight = 250;
  notifyRows(list, [19]);
  flushFrames();
  assert.equal(list.scrollTop, 2175);
  assert.equal(list._findIndexAtPosition(list.scrollTop), 20);
  assert.equal(list.scrollTop - list._getItemPosition(20), 25);
  list.items.get(21).offsetHeight = 200;
  notifyRows(list, [21]);
  flushFrames();
  assert.equal(list.scrollTop, 2175, "changes below the anchor must not move it");
});

test("measurement uses the latest native scroll position rather than a stale scroll frame", () => {
  const list = listWith(100);
  flushFrames();
  list.scrollToIndex(20);
  flushFrames();
  list.items.get(19).offsetHeight = 200;
  notifyRows(list, [19]);
  // A wheel event arrives after resize notification but before its RAF.
  list.container.scrollTop = 2225;
  list.container.listeners.get("scroll")();
  flushFrames();
  assert.equal(list.scrollTop, 2325);
  assert.equal(list._findIndexAtPosition(list.scrollTop), 22);
  assert.equal(list.scrollTop - list._getItemPosition(22), 25);
});

test("collapsing the anchor itself clamps its offset into the remaining row", () => {
  const list = listWith(100);
  flushFrames();
  list.scrollToIndex(20);
  list.container.scrollTop += 75;
  list._onScroll();
  flushFrames();
  list.items.get(20).offsetHeight = 10;
  notifyRows(list, [20]);
  flushFrames();
  assert.equal(list.scrollTop, 2009);
  assert.equal(list._findIndexAtPosition(list.scrollTop), 20);
});

test("short measured rows fill the viewport without needing a user scroll", () => {
  const list = listWith(100, {
    renderItem: () => Object.assign(new Element(), { offsetHeight: 20 }),
  });
  flushFrames();
  const end = list._findIndexAtPosition(list.containerHeight);
  assert.equal(end, 15);
  for (let index = 0; index <= end; index += 1) assert.ok(list.items.has(index));
  assert.ok(list.items.size <= 18);
});

test("width reflow invalidates offscreen heights while retaining the reader's anchor", () => {
  const list = listWith(100);
  flushFrames();
  list.items.get(0).offsetHeight = 170;
  notifyRows(list, [0]);
  flushFrames();
  list.scrollToIndex(50);
  list.container.scrollTop += 25;
  list._onScroll();
  flushFrames();
  assert.equal(list.heights.get(0), 170);
  list.container.clientWidth = 400;
  for (const element of list.items.values()) element.offsetHeight = 180;
  list._resizeObserver.notify();
  flushFrames();
  assert.equal(list.heights.has(0), false, "old wrapping measurements must be discarded");
  assert.equal(list._findIndexAtPosition(list.scrollTop), 50);
  assert.equal(list.scrollTop - list._getItemPosition(50), 25);
  assert.equal(list.items.get(50).style.top, `${list._getItemPosition(50)}px`);
});

test("hidden rows keep estimates and are measured when shown again", () => {
  const list = listWith(100);
  flushFrames();
  const row = list.items.get(0);
  row.offsetHeight = 0;
  notifyRows(list, [0]);
  flushFrames();
  assert.equal(list._getItemPosition(1), 100);
  row.offsetHeight = 220;
  notifyRows(list, [0]);
  flushFrames();
  assert.equal(list._getItemPosition(1), 220);
});

test("recycled elements are unobserved and pending stale callbacks cannot replace refreshed rows", () => {
  const list = listWith(100);
  flushFrames();
  const oldRow = list.items.get(0);
  oldRow.offsetHeight = 900;
  notifyRows(list, [0]);
  list.refresh();
  assert.ok(!list._itemResizeObserver.observed.has(oldRow));
  list.container.appendChild(oldRow);
  list._itemResizeObserver.notify([oldRow]);
  flushFrames();
  assert.equal(list._getItemPosition(1), 100);
  list.scrollToIndex(80);
  flushFrames();
  assert.deepEqual(new Set(list.items.values()), list._itemResizeObserver.observed);
  assert.ok(list._itemResizeObserver.observed.size <= 7);
});

test("destroy cancels queued scroll/measurement work and disconnects both observers", () => {
  const list = listWith(100);
  const row = list.items.get(0);
  const itemObserver = list._itemResizeObserver;
  const containerObserver = list._resizeObserver;
  list.container.listeners.get("scroll")();
  assert.equal(frames.size, 2);
  const lateCallbacks = [...frames.values()];
  list.destroy();
  assert.equal(frames.size, 0);
  assert.equal(itemObserver.observed.size, 0);
  assert.equal(containerObserver.observed.size, 0);
  assert.equal(list._pendingMeasurements.size, 0);
  assert.equal(list.container.listeners.size, 0);
  for (const callback of lateCallbacks) callback();
  itemObserver.notify([row]);
  containerObserver.notify([list.container]);
  assert.equal(frames.size, 0);
  assert.equal(list.items.size, 0);
  assert.equal(list._heightTree.size, 0);
});
