/**
 * Run: node --experimental-vm-modules --test tests/pages-pagination.test.mjs
 * Requires Node >=22.13 (node:sqlite). Runs the shipped query code against real
 * SQLite/FTS5 through a sqlite-wasm statement adapter. This is not browser E2E
 * coverage and does not exercise WASM initialization or encrypted loading.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { DatabaseSync } from 'node:sqlite';
import { test } from 'node:test';
import vm from 'node:vm';

const databaseSource = await readFile(new URL('../src/pages_assets/database.js', import.meta.url), 'utf8');

async function fixture(t, count = 137, globals = {}) {
    const sqlite = new DatabaseSync(':memory:');
    t.after(() => sqlite.close());
    sqlite.exec(`
        CREATE TABLE conversations (
            id INTEGER PRIMARY KEY, agent TEXT, workspace TEXT, title TEXT,
            source_path TEXT, started_at INTEGER, ended_at INTEGER,
            message_count INTEGER, metadata_json TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY, conversation_id INTEGER, idx INTEGER,
            role TEXT, content TEXT, created_at INTEGER, updated_at INTEGER, model TEXT
        );
        CREATE VIRTUAL TABLE messages_fts USING fts5(content, tokenize='porter unicode61');
        CREATE VIRTUAL TABLE messages_code_fts USING fts5(content, tokenize='unicode61');
    `);
    const conv = sqlite.prepare('INSERT INTO conversations VALUES (?, ?, ?, ?, ?, ?, NULL, 1, NULL)');
    const message = sqlite.prepare("INSERT INTO messages VALUES (?, ?, 0, 'user', ?, NULL, NULL, NULL)");
    const prose = sqlite.prepare('INSERT INTO messages_fts(rowid, content) VALUES (?, ?)');
    const code = sqlite.prepare('INSERT INTO messages_code_fts(rowid, content) VALUES (?, ?)');
    for (let id = 1; id <= count; id += 1) {
        conv.run(id, id % 2 ? 'codex' : 'claude', id % 2 ? '/one' : '/two', `Session ${id}`, `/logs/${id}`, 1000);
        const content = 'pagination needle search_target';
        message.run(id, id, content);
        prose.run(id, content);
        code.run(id, content);
    }
    sqlite.exec('PRAGMA query_only=ON');
    const queries = [];
    let finalized = 0;
    const handle = {
        prepare(sql) {
            const statement = sqlite.prepare(sql);
            let params = [];
            let iterator;
            let row;
            const execution = { sql, params, rows: 0 };
            queries.push(execution);
            return {
                bind(values) { params = values; execution.params = Array.from(values); },
                step() {
                    iterator ??= statement.iterate(...params);
                    const next = iterator.next();
                    row = next.value;
                    if (!next.done) execution.rows += 1;
                    return !next.done;
                },
                get(column) { return typeof column === 'number' ? Object.values(row)[column] : { ...row }; },
                finalize() { iterator?.return(); finalized += 1; },
            };
        },
    };
    // Inject only the database handle into an otherwise unchanged source file.
    // Tests do not replace any production SQL, filtering or ranking logic.
    const context = vm.createContext({ console, URL, ...globals });
    const module = new vm.SourceTextModule(`${databaseSource}\nexport function attachTestDatabase(value) { db = value; }`, { context });
    await module.link(() => { throw new Error('Unexpected static import'); });
    await module.evaluate();
    module.namespace.attachTestDatabase(handle);
    return { api: module.namespace, module, context, sqlite, queries, finalized: () => finalized };
}

function ids(rows, key = 'id') {
    return Array.from(rows, row => row[key]);
}

for (const searchMode of ['prose', 'code', 'auto']) {
    test(`${searchMode} search reaches every match in stable non-overlapping pages`, async t => {
        const { api, finalized } = await fixture(t);
        const query = searchMode === 'prose' ? 'needle' : 'search_target';
        const all = [0, 50, 100].flatMap(offset => ids(api.searchConversations(query, { searchMode, limit: 50, offset }), 'message_id'));
        assert.deepEqual(all, Array.from({ length: 137 }, (_, i) => i + 1));
        assert.deepEqual(ids(api.searchConversations(query, { searchMode, offset: 50 }), 'message_id'), all.slice(50, 100));
        assert.equal(api.searchConversations(query, { searchMode, offset: 150 }).length, 0);
        assert.equal(finalized(), 5);
    });
}

test('recent browsing is deterministic even when all timestamps are tied', async t => {
    const { api } = await fixture(t);
    const all = [0, 50, 100].flatMap(offset => ids(api.getRecentConversations(50, offset)));
    assert.deepEqual(all, Array.from({ length: 137 }, (_, i) => 137 - i));
    assert.equal(api.getRecentConversations(50, 150).length, 0);
});

test('agent, workspace and optional time filters apply before page boundaries', async t => {
    const { api } = await fixture(t);
    const expected = Array.from({ length: 69 }, (_, i) => 137 - 2 * i);
    for (const load of [
        offset => api.getConversationsByAgent('codex', 50, 1000, 1000, offset),
        offset => api.getConversationsByWorkspace('/one', 50, offset),
    ]) {
        assert.deepEqual([...ids(load(0)), ...ids(load(50))], expected);
        assert.equal(load(100).length, 0);
    }
    assert.deepEqual(ids(api.getConversationsByTimeRange(1000, 1000, 50, 50)), Array.from({ length: 50 }, (_, i) => 87 - i));
    assert.equal(api.getConversationsByAgent('codex', 50, 1001, null, 0).length, 0);
    assert.equal(api.getConversationsByTimeRange(null, 999, 50, 0).length, 0);
    assert.equal(api.getConversationsByTimeRange(1000, null, 50, 100).length, 37);
});

test('combined full-text filters retain every selected result across pages', async t => {
    const { api } = await fixture(t);
    const options = { agent: 'codex', since: 1000, until: 1000, limit: 50 };
    const all = [0, 50].flatMap(offset => ids(api.searchConversations('needle', { ...options, offset }), 'message_id'));
    assert.deepEqual(all, Array.from({ length: 69 }, (_, i) => 2 * i + 1));
    assert.equal(api.searchConversations('needle', { ...options, since: 1001 }).length, 0);
});

test('unsafe pagination cannot disable LIMIT or silently change the page', async t => {
    const { api, finalized } = await fixture(t);
    const loaders = [
        (limit, offset) => api.searchConversations('needle', { limit, offset }),
        (limit, offset) => api.getRecentConversations(limit, offset),
        (limit, offset) => api.getConversationsByAgent('codex', limit, null, null, offset),
        (limit, offset) => api.getConversationsByWorkspace('/one', limit, offset),
        (limit, offset) => api.getConversationsByTimeRange(null, null, limit, offset),
    ];
    for (const load of loaders) {
        for (const invalid of [-1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, '50', null]) {
            assert.throws(() => load(invalid, 0), /limit/i);
            assert.throws(() => load(50, invalid), /offset/i);
        }
        assert.throws(() => load(1001, 0), /limit/i);
        assert.equal(load(0, 0).length, 0);
    }
    assert.equal(finalized(), loaders.length, 'rejected requests must not execute SQL');
});

test('database errors are distinguishable from an empty result set', async t => {
    const { api, sqlite } = await fixture(t);
    assert.equal(api.searchConversations('absentword').length, 0);
    sqlite.exec('PRAGMA query_only=OFF; DROP TABLE messages_code_fts; PRAGMA query_only=ON');
    assert.throws(() => api.searchConversations('search_target'), /messages_code_fts/);
});

test('punctuation is parameterized and the archive remains read-only', async t => {
    const { api, sqlite } = await fixture(t);
    assert.equal(api.searchConversations('" OR 1=1; --').length, 0);
    assert.equal(sqlite.prepare('SELECT COUNT(*) AS n FROM conversations').get().n, 137);
    assert.throws(() => api.execute('DELETE FROM conversations'), /read-only/);
    assert.throws(() => api.queryAll('DELETE FROM conversations'), /readonly/i);
});

// The remaining tests execute search.js and database.js together. These small
// DOM/timer/virtual-list doubles test controller behavior and event wiring,
// NOT browser layout, screen-reader behavior or the VirtualList implementation.
function testDom() {
    const escape = text => String(text).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
    const document = { activeElement: null };
    class Element {
        constructor(tag = 'div') {
            this.tagName = tag;
            this.children = [];
            this.parent = null;
            this.attributes = {};
            this.style = {};
            this.value = '';
            this.disabled = false;
            this.scrollTop = 0;
            this._html = '';
            this._text = '';
            this._classes = new Set();
            this.listeners = new Map();
            this.dataset = new Proxy({}, { set(target, key, value) { target[key] = String(value); return true; } });
            this.classList = {
                add: (...names) => names.forEach(name => this._classes.add(name)),
                remove: (...names) => names.forEach(name => this._classes.delete(name)),
                contains: name => this._classes.has(name),
                toggle: (name, force = !this._classes.has(name)) => {
                    if (force) this._classes.add(name); else this._classes.delete(name);
                    return force;
                },
            };
        }
        set className(value) { this._classes = new Set(value.split(/\s+/)); }
        get className() { return [...this._classes].join(' '); }
        get options() { return this.children.filter(child => child.tagName === 'option'); }
        set textContent(value) { this._text = String(value); this._html = escape(value); this.children = []; }
        get textContent() { return this._text; }
        get innerHTML() { return this._html; }
        set innerHTML(html) {
            this._html = html;
            this._text = '';
            this.children = [];
            const stack = [this];
            // Only HTML emitted by this component is parsed. This is not an
            // HTML sanitizer/parser under test and must not be used in prod.
            for (const token of html.matchAll(/<!--[^]*?-->|<\/?([a-z][\w-]*)\b([^>]*?)>/gi)) {
                if (!token[1]) continue;
                if (token[0].startsWith('</')) { if (stack.length > 1) stack.pop(); continue; }
                const element = new Element(token[1]);
                for (const attr of token[2].matchAll(/([\w-]+)(?:="([^"]*)")?/g)) {
                    element.setAttribute(attr[1], attr[2] ?? '');
                }
                stack.at(-1).appendChild(element);
                if (!['input', 'br', 'hr', 'img', 'meta', 'link'].includes(element.tagName)) stack.push(element);
            }
        }
        setAttribute(name, value) {
            this.attributes[name] = String(value);
            if (name === 'class') this.className = value;
            if (name === 'id') this.id = value;
            if (name === 'value') this.value = value;
            if (name === 'disabled') this.disabled = true;
            if (name.startsWith('data-')) this.dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = value;
        }
        appendChild(child) { child.parent = this; this.children.push(child); return child; }
        remove() { if (this.parent) this.parent.children = this.parent.children.filter(child => child !== this); }
        matches(selector) {
            if (selector.startsWith('#')) return this.id === selector.slice(1);
            const match = /^\.([\w-]+)(?:\[data-result-index="(\d+)"\])?$/.exec(selector);
            return !!match && this.classList.contains(match[1]) && (match[2] === undefined || this.dataset.resultIndex === match[2]);
        }
        querySelectorAll(selector) {
            return this.children.flatMap(child => [...(child.matches(selector) ? [child] : []), ...child.querySelectorAll(selector)]);
        }
        querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
        closest(selector) { return this.matches(selector) ? this : this.parent?.closest(selector) ?? null; }
        addEventListener(type, callback) {
            if (!this.listeners.has(type)) this.listeners.set(type, []);
            this.listeners.get(type).push(callback);
        }
        emit(type, extra = {}) {
            const event = { type, target: this, preventDefault() {}, ...extra };
            for (let node = this; node; node = node.parent) {
                for (const callback of node.listeners.get(type) ?? []) callback(event);
            }
        }
        click() { if (!this.disabled) this.emit('click'); }
        focus() { document.activeElement = this; }
    }
    const root = new Element();
    document.createElement = tag => new Element(tag);
    document.getElementById = id => root.querySelector(`#${id}`);
    class VirtualList {
        constructor({ container, totalCount, renderItem }) {
            this.container = container;
            for (let i = 0; i < totalCount; i += 1) container.appendChild(renderItem(i));
        }
        destroy() { this.container.innerHTML = ''; }
        scrollToIndex() {}
    }
    return { document, root, VirtualList };
}

function testTimers() {
    let now = 0;
    let serial = 0;
    const tasks = new Map();
    return {
        setTimeout(callback, delay = 0) { const id = ++serial; tasks.set(id, { callback, due: now + delay }); return id; },
        clearTimeout(id) { tasks.delete(id); },
        async advance(ms = 0) {
            const end = now + ms;
            for (;;) {
                const next = [...tasks].filter(([, task]) => task.due <= end).sort((a, b) => a[1].due - b[1].due || a[0] - b[0])[0];
                if (!next) break;
                tasks.delete(next[0]);
                now = next[1].due;
                next[1].callback();
                await Promise.resolve();
                await Promise.resolve();
            }
            now = end;
        },
    };
}

async function searchFixture(t, count = 137) {
    const dom = testDom();
    const timers = testTimers();
    const errors = [];
    const db = await fixture(t, count, {
        document: dom.document,
        setTimeout: timers.setTimeout,
        clearTimeout: timers.clearTimeout,
        console: { debug() {}, warn() {}, log() {}, error(...args) { errors.push(args); } },
    });
    const router = new vm.SyntheticModule(['parseRouteIdSegment'], function () {
        this.setExport('parseRouteIdSegment', value => /^\d+$/.test(value) && Number.isSafeInteger(Number(value)) ? Number(value) : null);
    }, { context: db.context });
    const virtual = new vm.SyntheticModule(['VirtualList'], function () {
        this.setExport('VirtualList', dom.VirtualList);
    }, { context: db.context });
    const source = await readFile(new URL('../src/pages_assets/search.js', import.meta.url), 'utf8');
    const search = new vm.SourceTextModule(source, { context: db.context });
    await search.link(name => {
        const dependency = { './database.js': db.module, './router.js': router, './virtual-list.js': virtual }[name];
        assert.ok(dependency, `unexpected import: ${name}`);
        return dependency;
    });
    await search.evaluate();
    const selected = [];
    search.namespace.initSearch(dom.root, (...args) => selected.push(args));
    const element = id => dom.document.getElementById(id);
    return {
        ...db, ...dom, timers, errors, selected, search: search.namespace, element,
        resultIds: () => element('results-list').querySelectorAll('.result-card').map(card => Number(card.dataset.conversationId)),
        async run(action) { const pending = action(); await timers.advance(); await pending; },
        async click(id) { element(id).click(); await timers.advance(); },
    };
}

test('UI reaches results beyond 1000 while retaining only one page', async t => {
    const f = await searchFixture(t, 1053);
    await f.run(() => f.search.setSearchQuery('needle'));
    assert.match(f.element('result-count').textContent, /1–50.*more available/);
    const all = [...f.resultIds()];
    for (let page = 1; f.search.getSearchState().hasNextPage; page += 1) {
        await f.click('search-next-page');
        assert.equal(f.search.getSearchState().page, page + 1);
        assert.ok(f.search.getSearchState().resultCount <= 50);
        all.push(...f.resultIds());
        assert.equal(f.document.activeElement?.dataset.resultIndex, '0');
        assert.ok(page < 30, 'navigation must terminate');
    }
    assert.deepEqual(all, Array.from({ length: 1053 }, (_, i) => i + 1));
    assert.match(f.element('result-count').textContent, /1051–1053 of 1053/);
    assert.equal(f.element('search-next-page').disabled, true);
    const pageQueries = f.queries.filter(q => q.sql.includes('LIMIT'));
    assert.equal(pageQueries.length, 22);
    assert.ok(pageQueries.every(q => q.rows <= 51 && q.params.at(-2) === 51));
    await f.click('search-previous-page');
    assert.deepEqual(f.resultIds(), Array.from({ length: 50 }, (_, i) => i + 1001));
});

for (const count of [0, 1, 20, 21, 50, 51, 100]) {
    test(`UI has exact navigation boundaries for ${count} matches`, async t => {
        const f = await searchFixture(t, count);
        await f.run(() => f.search.setSearchQuery('needle'));
        assert.equal(f.search.getSearchState().resultCount, Math.min(count, 50));
        assert.equal(f.element('search-previous-page').disabled, true);
        assert.equal(f.element('search-next-page').disabled, count <= 50);
        assert.equal(f.element('search-pagination').classList.contains('hidden'), count <= 50);
        if (count > 50) {
            await f.click('search-next-page');
            assert.equal(f.search.getSearchState().resultCount, count - 50);
            assert.equal(f.element('search-next-page').disabled, true);
            await f.click('search-previous-page');
            assert.equal(f.search.getSearchState().page, 1);
        }
    });
}

test('UI pages filtered browsing and search with either time bound omitted', async t => {
    const f = await searchFixture(t);
    for (const route of [
        { query: '' },
        { query: '', since: 1000 },
        { query: '', until: 1000 },
        { query: '', agent: 'codex', since: 1000 },
        { query: 'needle', agent: 'codex', until: 1000 },
    ]) {
        await f.run(() => f.search.setSearchRoute(route));
        const all = [...f.resultIds()];
        while (f.search.getSearchState().hasNextPage) {
            await f.click('search-next-page');
            all.push(...f.resultIds());
        }
        const selected = Array.from({ length: 137 }, (_, i) => i + 1).filter(id => !route.agent || id % 2 === 1);
        assert.deepEqual(all, route.query ? selected : selected.reverse());
    }
});

test('new queries supersede pending work and cannot reuse a previous offset', async t => {
    const f = await searchFixture(t);
    await f.run(() => f.search.setSearchQuery('needle'));
    await f.click('search-next-page');
    const before = f.queries.length;
    const old = f.search.setSearchQuery('search_target');
    const latest = f.search.setSearchQuery('absentword');
    await f.timers.advance();
    await Promise.all([old, latest]);
    assert.equal(f.search.getSearchState().page, 1);
    assert.equal(f.search.getSearchState().resultCount, 0);
    assert.equal(f.queries.length, before + 1, 'superseded query must not reach SQLite');
    assert.equal(f.queries.at(-1).params[0], '"absentword"');
    assert.equal(f.element('no-results').classList.contains('hidden'), false);
});

test('session lock cancels pending pages and announcements without retaining results', async t => {
    const f = await searchFixture(t);
    await f.run(() => f.search.setSearchQuery('needle'));
    const before = f.queries.length;
    f.element('search-next-page').click();
    f.search.clearSearch({ reloadRecent: false });
    await f.timers.advance(1000);
    assert.equal(f.queries.length, before);
    assert.equal(f.search.getSearchState().resultCount, 0);
    assert.equal(f.search.getSearchState().isSearching, false);
    assert.equal(f.search.getSearchState().hasNextPage, false);
    assert.equal(f.element('results-list').innerHTML, '');
    assert.equal(f.element('search-announcer').textContent, '');
    assert.equal(f.element('search-next-page').disabled, true);
});

test('database failure clears stale hits and exposes an error rather than no results', async t => {
    const f = await searchFixture(t);
    await f.run(() => f.search.setSearchQuery('needle'));
    await f.click('search-next-page');
    f.sqlite.exec('PRAGMA query_only=OFF; DROP TABLE messages_code_fts; PRAGMA query_only=ON');
    await f.run(() => f.search.setSearchQuery('search_target'));
    assert.equal(f.search.getSearchState().resultCount, 0);
    assert.equal(f.search.getSearchState().hasNextPage, false);
    assert.equal(f.search.getSearchState().isSearching, false);
    assert.match(f.element('results-list').innerHTML, /role="alert"/);
    assert.equal(f.element('no-results').classList.contains('hidden'), true);
    assert.equal(f.errors.length, 1);
    await f.run(() => f.search.setSearchQuery('needle'));
    assert.equal(f.search.getSearchState().resultCount, 50);
});

test('filter changes use pending input and cancel its debounce instead of searching the old query', async t => {
    const f = await searchFixture(t);
    await f.run(() => f.search.setSearchQuery('needle'));
    f.element('search-input').value = 'absentword';
    f.element('search-input').emit('input');
    f.element('agent-filter').value = 'codex';
    f.element('agent-filter').emit('change');
    await f.timers.advance();
    const before = f.queries.length;
    await f.timers.advance(500);
    assert.equal(f.queries.length, before);
    assert.equal(f.search.getSearchState().query, 'absentword');
    assert.equal(f.search.getSearchState().filters.agent, 'codex');
    assert.equal(f.search.getSearchState().resultCount, 0);
});

test('explicit search cancels debounce and rapid Next clicks cannot skip a page', async t => {
    const f = await searchFixture(t);
    f.element('search-input').value = 'needle';
    f.element('search-input').emit('input');
    await f.click('search-btn');
    const before = f.queries.length;
    f.element('search-next-page').emit('click');
    f.element('search-next-page').emit('click');
    assert.equal(f.element('search-next-page').disabled, true);
    await f.timers.advance(1000);
    assert.equal(f.queries.length, before + 1);
    assert.equal(f.search.getSearchState().page, 2);
    assert.equal(f.element('search-next-page').disabled, false);
});

test('deferred query and route setters invalidate pending renders without running SQL', async t => {
    const f = await searchFixture(t);
    for (const deferred of [
        () => f.search.setSearchQuery('pending', { runSearch: false }),
        () => f.search.setSearchRoute({ query: 'pending', agent: 'codex' }, { runSearch: false }),
    ]) {
        const before = f.queries.length;
        const old = f.search.setSearchQuery('needle');
        await deferred();
        await f.timers.advance(500);
        await old;
        assert.equal(f.queries.length, before);
        assert.equal(f.search.getSearchState().resultCount, 0);
        assert.equal(f.element('results-list').innerHTML, '');
    }
});

test('virtual result pointer and keyboard activation each select exactly once', async t => {
    const f = await searchFixture(t);
    await f.run(() => f.search.setSearchQuery('needle'));
    const card = f.element('results-list').querySelector('.result-card');
    card.click();
    assert.deepEqual(f.selected, [[1, 1]]);
    card.focus();
    card.emit('keydown', { key: 'Enter' });
    assert.deepEqual(f.selected, [[1, 1], [1, 1]]);
});
