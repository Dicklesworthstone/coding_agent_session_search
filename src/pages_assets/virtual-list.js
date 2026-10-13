/**
 * cass Archive Virtual List Component
 *
 * Efficient virtual scrolling that renders only visible items, enabling
 * smooth navigation through 10K+ search results without memory exhaustion.
 *
 * Uses a fixed-height item approach for O(1) scroll position calculations.
 */

/**
 * Virtual List for fixed-height items
 *
 * @example
 * const list = new VirtualList({
 *     container: document.getElementById('results'),
 *     itemHeight: 80,
 *     totalCount: results.length,
 *     renderItem: (index) => createResultCard(results[index]),
 *     overscan: 3
 * });
 */
export class VirtualList {
  /**
   * Create a new virtual list
   * @param {Object} options
   * @param {HTMLElement} options.container - Scroll container element
   * @param {number} options.itemHeight - Fixed height per item in pixels
   * @param {number} options.totalCount - Total number of items
   * @param {Function} options.renderItem - Function to render item at index
   * @param {number} [options.overscan=3] - Extra items to render above/below viewport
   * @param {Function} [options.onScrollEnd] - Callback when near end of list
   */
  constructor({ container, itemHeight, totalCount, renderItem, overscan = 3, onScrollEnd = null }) {
    this.container = container;
    this.itemHeight = itemHeight;
    this.totalCount = totalCount;
    this.renderItem = renderItem;
    this.overscan = overscan;
    this.onScrollEnd = onScrollEnd;

    this.scrollTop = 0;
    this.containerHeight = 0;
    this.items = new Map(); // index -> element
    this.lastVisibleRange = { start: -1, end: -1 };

    // Performance metrics
    this.metrics = {
      renders: 0,
      recycled: 0,
      created: 0,
    };
    this.destroyed = false;

    this._init();
  }

  /**
   * Initialize the virtual list
   * @private
   */
  _init() {
    // Create inner container for total height simulation
    this.inner = document.createElement("div");
    this.inner.className = "virtual-list-inner";
    this.inner.style.height = `${this.totalCount * this.itemHeight}px`;
    this.inner.style.position = "relative";
    this.inner.style.width = "100%";

    // Clear and set up container
    this.container.innerHTML = "";
    this.container.style.overflow = "auto";
    this.container.style.position = "relative";
    this.container.appendChild(this.inner);

    // Set up resize observer for responsive sizing
    this._resizeObserver = new ResizeObserver(() => this._onResize());
    this._resizeObserver.observe(this.container);

    // Throttled scroll handler
    this._scrollHandler = this._createThrottledHandler(() => this._onScroll(), 16);
    this.container.addEventListener("scroll", this._scrollHandler, { passive: true });

    // Initial render
    this._onResize();

    console.debug("[VirtualList] Initialized with", this.totalCount, "items");
  }

  /**
   * Create a throttled event handler using requestAnimationFrame
   * @private
   */
  _createThrottledHandler(fn, wait) {
    let pending = false;
    return () => {
      if (this.destroyed || pending) {
        return;
      }
      if (!pending) {
        pending = true;
        requestAnimationFrame(() => {
          if (this.destroyed) {
            pending = false;
            return;
          }
          fn();
          pending = false;
        });
      }
    };
  }

  /**
   * Handle container resize
   * @private
   */
  _onResize() {
    if (this.destroyed) {
      return;
    }
    this.containerHeight = this.container.clientHeight;
    this._render();
  }

  /**
   * Handle scroll event
   * @private
   */
  _onScroll() {
    if (this.destroyed) {
      return;
    }
    this.scrollTop = this.container.scrollTop;
    this._render();

    // Check for scroll end callback
    if (this.onScrollEnd && this._isNearEnd()) {
      this.onScrollEnd();
    }
  }

  /**
   * Check if scrolled near the end
   * @private
   */
  _isNearEnd() {
    const totalHeight = this.totalCount * this.itemHeight;
    const remaining = totalHeight - this.scrollTop - this.containerHeight;
    return remaining < this.containerHeight * 2;
  }

  /**
   * Calculate visible range of items
   * @private
   */
  _getVisibleRange() {
    const startIndex = Math.max(0, Math.floor(this.scrollTop / this.itemHeight) - this.overscan);
    const endIndex = Math.min(
      this.totalCount,
      Math.ceil((this.scrollTop + this.containerHeight) / this.itemHeight) + this.overscan,
    );
    return { start: startIndex, end: endIndex };
  }

  /**
   * Render visible items
   * @private
   */
  _render() {
    if (this.destroyed || !this.inner) {
      return;
    }
    const { start, end } = this._getVisibleRange();

    // Skip render if range unchanged
    if (start === this.lastVisibleRange.start && end === this.lastVisibleRange.end) {
      return;
    }

    this.lastVisibleRange = { start, end };
    this.metrics.renders++;

    const visible = new Set();

    // Add/update visible items
    for (let i = start; i < end; i++) {
      visible.add(i);

      if (!this.items.has(i)) {
        const element = this.renderItem(i);
        element.style.position = "absolute";
        element.style.top = `${i * this.itemHeight}px`;
        element.style.left = "0";
        element.style.right = "0";
        element.style.height = `${this.itemHeight}px`;
        element.dataset.virtualIndex = i;

        this.inner.appendChild(element);
        this.items.set(i, element);
        this.metrics.created++;
      }
    }

    // Remove items no longer visible
    for (const [index, element] of this.items) {
      if (!visible.has(index)) {
        element.remove();
        this.items.delete(index);
        this.metrics.recycled++;
      }
    }

    console.debug(
      `[VirtualList] Rendering ${this.items.size} of ${this.totalCount} items (range: ${start}-${end})`,
    );
  }

  /**
   * Update total item count
   * @param {number} newCount - New total count
   */
  updateTotalCount(newCount) {
    this.totalCount = newCount;
    this.inner.style.height = `${newCount * this.itemHeight}px`;

    // Force re-render to clean up out-of-range items
    this.lastVisibleRange = { start: -1, end: -1 };
    this._render();
  }

  /**
   * Scroll to a specific item index
   * @param {number} index - Item index to scroll to
   * @param {string} [align='start'] - Alignment: 'start' | 'center' | 'end'
   */
  scrollToIndex(index, align = "start") {
    let targetTop = index * this.itemHeight;

    if (align === "center") {
      targetTop = targetTop - this.containerHeight / 2 + this.itemHeight / 2;
    } else if (align === "end") {
      targetTop = targetTop - this.containerHeight + this.itemHeight;
    }

    this.container.scrollTop = Math.max(0, targetTop);
    this.scrollTop = this.container.scrollTop;
    this._render();
  }

  /**
   * Force re-render all visible items
   */
  refresh() {
    // Remove all current items
    for (const [, element] of this.items) {
      element.remove();
    }
    this.items.clear();
    this.lastVisibleRange = { start: -1, end: -1 };
    this._render();
  }

  /**
   * Get the currently visible range
   * @returns {{start: number, end: number}} Visible item range
   */
  getVisibleRange() {
    return { ...this.lastVisibleRange };
  }

  /**
   * Get performance metrics
   * @returns {Object} Metrics object
   */
  getMetrics() {
    return { ...this.metrics };
  }

  /**
   * Clean up resources
   */
  destroy() {
    this.destroyed = true;

    if (this._resizeObserver) {
      this._resizeObserver.disconnect();
      this._resizeObserver = null;
    }

    if (this._scrollHandler) {
      this.container.removeEventListener("scroll", this._scrollHandler);
      this._scrollHandler = null;
    }

    for (const [, element] of this.items) {
      element.remove();
    }
    this.items.clear();

    if (this.inner) {
      this.inner.remove();
      this.inner = null;
    }

    console.debug("[VirtualList] Destroyed. Metrics:", this.metrics);
  }
}

// The Fenwick low-bit operation uses signed 32-bit indices. This is an
// arithmetic bound, not a promise to exceed a browser's scroll-canvas limit.
function validateVirtualCount(count, estimatedHeight) {
  if (!Number.isSafeInteger(count) || count < 0 || count > 0x7fffffff) {
    throw new RangeError("Virtual list count must be an integer between 0 and 2147483647");
  }
  if (!Number.isFinite(estimatedHeight) || estimatedHeight <= 0 ||
      count * estimatedHeight > Number.MAX_SAFE_INTEGER) {
    throw new RangeError("Virtual list estimated height must be positive with a safe total height");
  }
}

/**
 * Virtual List for variable-height items
 *
 * Uses estimated heights and measures actual heights after rendering.
 * More expensive than fixed-height but handles dynamic content.
 *
 * @example
 * const list = new VariableHeightVirtualList({
 *     container: document.getElementById('messages'),
 *     totalCount: messages.length,
 *     estimatedItemHeight: 120,
 *     renderItem: (index) => createMessageElement(messages[index]),
 * });
 */
export class VariableHeightVirtualList {
  /**
   * Create a new variable-height virtual list
   * @param {Object} options
   * @param {HTMLElement} options.container - Scroll container element
   * @param {number} options.totalCount - Total number of items
   * @param {number} options.estimatedItemHeight - Estimated height per item
   * @param {Function} options.renderItem - Function to render item at index
   * @param {number} [options.overscan=5] - Extra items to render above/below viewport
   */
  constructor({ container, totalCount, estimatedItemHeight, renderItem, overscan = 5 }) {
    validateVirtualCount(totalCount, estimatedItemHeight);
    if (!Number.isSafeInteger(overscan) || overscan < 0) {
      throw new RangeError("Virtual list overscan must be a non-negative safe integer");
    }
    this.container = container;
    this.totalCount = totalCount;
    this.estimatedHeight = estimatedItemHeight;
    this.renderItem = renderItem;
    this.overscan = overscan;

    this.scrollTop = 0;
    this.containerHeight = 0;

    // Height tracking
    this.heights = new Map(); // index -> measured height
    // Sparse Fenwick tree of deviations from the estimated height. Opening a
    // large transcript allocates no entry for an unseen row. Measurements and
    // offset lookups take O(log N), not a walk over every following message.
    this._heightTree = new Map();

    // DOM tracking
    this.items = new Map(); // index -> element
    this.lastVisibleRange = { start: -1, end: -1 };
    this.destroyed = false;
    this._scrollFramePending = false;
    this._scrollFrameId = null;
    this._measureFrameId = null;
    this._pendingMeasurements = new Map();
    this._containerWidth = null;

    this._init();
  }

  /**
   * Initialize the virtual list
   * @private
   */
  _init() {
    // Calculate initial positions
    this._calculatePositions();

    // Create inner container
    this.inner = document.createElement("div");
    this.inner.className = "virtual-list-inner variable-height";
    this.inner.style.position = "relative";
    this.inner.style.width = "100%";
    // This list maintains its own visible-row anchor across height edits.
    // Disable native anchoring for these absolutely positioned descendants.
    this.inner.style.overflowAnchor = "none";
    this._updateTotalHeight();

    // Set up container
    this.container.innerHTML = "";
    this.container.style.overflow = "auto";
    this.container.style.position = "relative";
    this.container.appendChild(this.inner);

    // Observe only mounted rows. Tool expansion, images, font loading and
    // wrapping may all change a message after its initial measurement.
    this._itemResizeObserver = new ResizeObserver((entries) => {
      for (const { target } of entries) {
        this._queueMeasurement(Number(target.dataset.virtualIndex), target);
      }
    });
    this._resizeObserver = new ResizeObserver(() => this._onResize());
    this._resizeObserver.observe(this.container);

    // Scroll handler
    this._scrollHandler = () => {
      if (this.destroyed || this._scrollFramePending) {
        return;
      }

      this._scrollFramePending = true;
      this._scrollFrameId = requestAnimationFrame(() => {
        this._scrollFramePending = false;
        this._scrollFrameId = null;
        if (this.destroyed) {
          return;
        }
        this._onScroll();
      });
    };
    this.container.addEventListener("scroll", this._scrollHandler, { passive: true });

    // Initial render
    this._onResize();

    console.debug("[VariableVirtualList] Initialized with", this.totalCount, "items");
  }

  /**
   * Calculate cumulative positions based on known/estimated heights
   * @private
   */
  _calculatePositions() {
    this._heightTree.clear();
    // Count changes rebuild only measured rows; removed rows must not regain
    // obsolete heights if the list subsequently grows again.
    for (const [index, height] of this.heights) {
      if (index >= this.totalCount) {
        this.heights.delete(index);
      } else {
        this._addHeightDelta(index, height - this.estimatedHeight);
      }
    }
  }

  _addHeightDelta(index, delta) {
    if (delta === 0) {
      return;
    }
    for (let node = index + 1; node <= this.totalCount; node += node & -node) {
      const value = (this._heightTree.get(node) ?? 0) + delta;
      if (value === 0) {
        this._heightTree.delete(node);
      } else {
        this._heightTree.set(node, value);
      }
    }
  }

  _setItemHeight(index, height) {
    // Hidden/detached elements report zero. Keeping a positive estimate also
    // preserves the monotonic prefix sums needed for offset lookup.
    if (!Number.isInteger(index) || index < 0 || index >= this.totalCount ||
        !Number.isFinite(height) || height <= 0) {
      return false;
    }
    const previous = this._getItemHeight(index);
    this.heights.set(index, height);
    this._addHeightDelta(index, height - previous);
    return previous !== height;
  }

  /**
   * Update total height based on positions
   * @private
   */
  _updateTotalHeight() {
    const totalHeight = this._getItemPosition(this.totalCount);
    this.inner.style.height = `${totalHeight}px`;
  }

  /**
   * Get item height (measured or estimated)
   * @private
   */
  _getItemHeight(index) {
    return this.heights.get(index) ?? this.estimatedHeight;
  }

  /**
   * Get item position
   * @private
   */
  _getItemPosition(index) {
    let position = index * this.estimatedHeight;
    for (let node = index; node > 0; node -= node & -node) {
      position += this._heightTree.get(node) ?? 0;
    }
    return position;
  }

  /**
   * Find item index at scroll position using binary search
   * @private
   */
  _findIndexAtPosition(scrollTop) {
    if (this.totalCount === 0 || !(scrollTop > 0)) {
      return 0;
    }
    // Binary lifting over the implicit estimated-height tree finds the last
    // prefix <= scrollTop in O(log N), including exact item boundaries.
    let index = 0;
    let position = 0;
    for (let step = 2 ** Math.floor(Math.log2(this.totalCount)); step >= 1; step /= 2) {
      const next = index + step;
      if (next <= this.totalCount) {
        const blockHeight = (next & -next) * this.estimatedHeight +
          (this._heightTree.get(next) ?? 0);
        if (position + blockHeight <= scrollTop) {
          index = next;
          position += blockHeight;
        }
      }
    }
    return Math.min(index, this.totalCount - 1);
  }

  _setScrollTop(top) {
    const maximum = Math.max(0, this._getItemPosition(this.totalCount) - this.containerHeight);
    this.container.scrollTop = Math.max(0, Math.min(top, maximum));
    this.scrollTop = this.container.scrollTop;
  }

  /**
   * Handle container resize
   * @private
   */
  _onResize() {
    if (this.destroyed) {
      return;
    }
    const width = this.container.clientWidth;
    const widthChanged = width > 0 && this._containerWidth !== null &&
      width !== this._containerWidth;
    this.containerHeight = this.container.clientHeight;
    if (width > 0) {
      this._containerWidth = width;
    }
    if (widthChanged) {
      // Offscreen measurements were taken at the old wrapping width. Keep
      // the reader's row, but do not reuse those heights when scrolling back.
      const anchor = this._captureAnchor();
      this.heights.clear();
      this._heightTree.clear();
      this._commitLayout(anchor);
    } else {
      this._setScrollTop(this.container.scrollTop);
      this._render();
    }
    for (const [index, element] of this.items) {
      this._queueMeasurement(index, element);
    }
  }

  /**
   * Handle scroll event
   * @private
   */
  _onScroll() {
    if (this.destroyed) {
      return;
    }
    this.scrollTop = this.container.scrollTop;
    this._render();
  }

  /**
   * Get visible range
   * @private
   */
  _getVisibleRange() {
    const startIndex = Math.max(0, this._findIndexAtPosition(this.scrollTop) - this.overscan);
    const endIndex = Math.min(
      this.totalCount,
      this._findIndexAtPosition(this.scrollTop + this.containerHeight) + this.overscan + 1,
    );
    return { start: startIndex, end: endIndex };
  }

  /**
   * Render visible items
   * @private
   */
  _render() {
    if (this.destroyed || !this.inner) {
      return;
    }
    const { start, end } = this._getVisibleRange();

    // Skip if unchanged
    if (start === this.lastVisibleRange.start && end === this.lastVisibleRange.end) {
      return;
    }

    this.lastVisibleRange = { start, end };
    const visible = new Set();

    // Add/update visible items
    for (let i = start; i < end; i++) {
      visible.add(i);

      if (!this.items.has(i)) {
        const element = this.renderItem(i);
        element.style.position = "absolute";
        element.style.top = `${this._getItemPosition(i)}px`;
        element.style.left = "0";
        element.style.right = "0";
        element.dataset.virtualIndex = i;

        this.inner.appendChild(element);
        this.items.set(i, element);

        this._itemResizeObserver.observe(element, { box: "border-box" });
        this._queueMeasurement(i, element);
      }
    }

    // Remove items no longer visible
    for (const [index, element] of this.items) {
      if (!visible.has(index)) {
        this._itemResizeObserver.unobserve(element);
        this._pendingMeasurements.delete(index);
        element.remove();
        this.items.delete(index);
      }
    }

    console.debug(`[VariableVirtualList] Rendering ${this.items.size} of ${this.totalCount} items`);
  }

  /**
   * Measure rendered item and update positions if needed
   * @private
   */
  _isMountedItem(index, element) {
    return !this.destroyed && this.inner && element?.isConnected &&
      this.items.get(index) === element;
  }

  _queueMeasurement(index, element) {
    if (!this._isMountedItem(index, element)) {
      return;
    }
    this._pendingMeasurements.set(index, element);
    if (this._measureFrameId === null) {
      this._measureFrameId = requestAnimationFrame(() => {
        this._measureFrameId = null;
        const pending = [...this._pendingMeasurements];
        this._pendingMeasurements.clear();
        this._applyMeasurements(pending);
      });
    }
  }

  _captureAnchor() {
    const top = Math.max(0, this.container.scrollTop);
    const index = this._findIndexAtPosition(top);
    return { index, offset: top - this._getItemPosition(index) };
  }

  _commitLayout(anchor) {
    this._updateTotalHeight();
    for (const [index, element] of this.items) {
      element.style.top = `${this._getItemPosition(index)}px`;
    }
    const offset = Math.min(anchor.offset, Math.max(0, this._getItemHeight(anchor.index) - 1));
    this._setScrollTop(this._getItemPosition(anchor.index) + offset);
    // Smaller measured rows can expose more messages even without a scroll
    // event. Recompute the window now rather than leaving a blank viewport.
    this._render();
  }

  _applyMeasurements(entries) {
    if (this.destroyed || !this.inner) {
      return;
    }
    const anchor = this._captureAnchor();
    // Batch all layout reads before any positioning writes. Observer events
    // in the same frame coalesce, and recycled/replaced elements are rejected.
    const measured = [];
    for (const [index, element] of entries) {
      if (this._isMountedItem(index, element)) {
        measured.push([index, element.offsetHeight]);
      }
    }
    let changed = false;
    for (const [index, height] of measured) {
      changed = this._setItemHeight(index, height) || changed;
    }
    if (changed) {
      this._commitLayout(anchor);
    }
  }

  _measureItem(index, element) {
    this._applyMeasurements([[index, element]]);
  }

  /**
   * Scroll to a specific item index
   * @param {number} index - Item index to scroll to
   * @param {string} [align='start'] - Alignment: 'start' | 'center' | 'end'
   */
  scrollToIndex(index, align = "start") {
    if (this.destroyed || !Number.isSafeInteger(index) || index < 0 || index >= this.totalCount) {
      return;
    }
    let targetTop = this._getItemPosition(index);
    const itemHeight = this._getItemHeight(index);

    if (align === "center") {
      targetTop = targetTop - this.containerHeight / 2 + itemHeight / 2;
    } else if (align === "end") {
      targetTop = targetTop - this.containerHeight + itemHeight;
    }

    this._setScrollTop(targetTop);
    this._render();
  }

  /**
   * Update total item count
   * @param {number} newCount - New total count
   */
  updateTotalCount(newCount) {
    if (this.destroyed) {
      return;
    }
    validateVirtualCount(newCount, this.estimatedHeight);
    this.totalCount = newCount;
    this._calculatePositions();
    this._updateTotalHeight();
    this._setScrollTop(this.container.scrollTop);
    this.lastVisibleRange = { start: -1, end: -1 };
    this._render();
  }

  /**
   * Force re-render all visible items
   */
  refresh() {
    if (this.destroyed) {
      return;
    }
    for (const [, element] of this.items) {
      this._itemResizeObserver.unobserve(element);
      element.remove();
    }
    this._pendingMeasurements.clear();
    this.items.clear();
    this.lastVisibleRange = { start: -1, end: -1 };
    this._render();
  }

  /**
   * Clean up resources
   */
  destroy() {
    if (this.destroyed) {
      return;
    }
    this.destroyed = true;
    this.heights.clear();
    this._heightTree.clear();
    this._pendingMeasurements.clear();
    this._itemResizeObserver.disconnect();
    if (this._measureFrameId !== null) {
      cancelAnimationFrame(this._measureFrameId);
      this._measureFrameId = null;
    }

    if (this._resizeObserver) {
      this._resizeObserver.disconnect();
    }

    if (this._scrollHandler) {
      this.container.removeEventListener("scroll", this._scrollHandler);
    }

    if (this._scrollFrameId !== null && typeof cancelAnimationFrame === "function") {
      cancelAnimationFrame(this._scrollFrameId);
      this._scrollFrameId = null;
    }
    this._scrollFramePending = false;

    for (const [, element] of this.items) {
      element.remove();
    }
    this.items.clear();

    if (this.inner) {
      this.inner.remove();
      this.inner = null;
    }

    console.debug("[VariableVirtualList] Destroyed");
  }
}

// Export default
export default {
  VirtualList,
  VariableHeightVirtualList,
};
