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
    this.container = container;
    this.totalCount = totalCount;
    this.estimatedHeight = estimatedItemHeight;
    this.renderItem = renderItem;
    this.overscan = overscan;

    this.scrollTop = 0;
    this.containerHeight = 0;

    // Height tracking
    this.heights = new Map(); // index -> measured height
    this.positions = []; // cumulative positions

    // DOM tracking
    this.items = new Map(); // index -> element
    this.lastVisibleRange = { start: -1, end: -1 };
    this.destroyed = false;
    this._scrollFramePending = false;
    this._scrollFrameId = null;

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
    this._updateTotalHeight();

    // Set up container
    this.container.innerHTML = "";
    this.container.style.overflow = "auto";
    this.container.style.position = "relative";
    this.container.appendChild(this.inner);

    // Set up resize observer
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
    this.positions = new Array(this.totalCount + 1);
    this.positions[0] = 0;

    for (let i = 0; i < this.totalCount; i++) {
      const height = this.heights.get(i) ?? this.estimatedHeight;
      this.positions[i + 1] = this.positions[i] + height;
    }
  }

  /**
   * Update total height based on positions
   * @private
   */
  _updateTotalHeight() {
    const totalHeight = this.positions[this.totalCount] ?? this.totalCount * this.estimatedHeight;
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
    return this.positions[index] ?? index * this.estimatedHeight;
  }

  /**
   * Find item index at scroll position using binary search
   * @private
   */
  _findIndexAtPosition(scrollTop) {
    let low = 0;
    let high = this.totalCount - 1;

    while (low < high) {
      const mid = Math.floor((low + high + 1) / 2);
      if (this._getItemPosition(mid) <= scrollTop) {
        low = mid;
      } else {
        high = mid - 1;
      }
    }

    return low;
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

        // Measure actual height after render
        requestAnimationFrame(() => {
          if (this.destroyed) {
            return;
          }
          this._measureItem(i, element);
        });
      }
    }

    // Remove items no longer visible
    for (const [index, element] of this.items) {
      if (!visible.has(index)) {
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
  _measureItem(index, element) {
    if (this.destroyed || !this.inner || !element?.isConnected) {
      return;
    }
    const measuredHeight = element.offsetHeight;
    const previousHeight = this.heights.get(index);

    if (previousHeight !== measuredHeight) {
      this.heights.set(index, measuredHeight);

      // Recalculate positions from this index forward
      for (let i = index; i < this.totalCount; i++) {
        const height = this.heights.get(i) ?? this.estimatedHeight;
        this.positions[i + 1] = this.positions[i] + height;
      }

      this._updateTotalHeight();

      // Update positions of rendered items after this one
      for (const [idx, el] of this.items) {
        if (idx > index) {
          el.style.top = `${this._getItemPosition(idx)}px`;
        }
      }
    }
  }

  /**
   * Scroll to a specific item index
   * @param {number} index - Item index to scroll to
   * @param {string} [align='start'] - Alignment: 'start' | 'center' | 'end'
   */
  scrollToIndex(index, align = "start") {
    let targetTop = this._getItemPosition(index);
    const itemHeight = this._getItemHeight(index);

    if (align === "center") {
      targetTop = targetTop - this.containerHeight / 2 + itemHeight / 2;
    } else if (align === "end") {
      targetTop = targetTop - this.containerHeight + itemHeight;
    }

    this.container.scrollTop = Math.max(0, targetTop);
    this.scrollTop = this.container.scrollTop;
    this._render();
  }

  /**
   * Update total item count
   * @param {number} newCount - New total count
   */
  updateTotalCount(newCount) {
    this.totalCount = newCount;
    this._calculatePositions();
    this._updateTotalHeight();
    this.lastVisibleRange = { start: -1, end: -1 };
    this._render();
  }

  /**
   * Force re-render all visible items
   */
  refresh() {
    for (const [, element] of this.items) {
      element.remove();
    }
    this.items.clear();
    this.lastVisibleRange = { start: -1, end: -1 };
    this._render();
  }

  /**
   * Clean up resources
   */
  destroy() {
    this.destroyed = true;

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
