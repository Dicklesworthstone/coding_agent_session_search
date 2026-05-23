/**
 * cass Archive Stats Dashboard Module
 *
 * Renders an instant analytics dashboard using precomputed JSON files
 * (statistics.json, timeline.json, agent_summary.json, workspace_summary.json, top_terms.json)
 * generated during export. Falls back to database queries if JSON not available.
 *
 * Routes:
 *   #/stats -> analytics dashboard
 */

import { queryAll, queryOne, queryValue, isDatabaseReady } from './database.js';

// State
let analyticsData = null;
let container = null;
let isLoading = false;
let currentTimelineView = 'monthly'; // 'daily' | 'weekly' | 'monthly'
let analyticsEpoch = 0;

// Cache for computed analytics (when using database fallback)
let computedAnalytics = null;

const ANALYTICS_FETCH_TIMEOUT_MS = 10000;

function isCurrentAnalyticsEpoch(epoch) {
    return epoch === analyticsEpoch;
}

/**
 * Initialize the stats module with a container element
 * @param {HTMLElement} containerElement - Container to render into
 */
export function initStats(containerElement) {
    container = containerElement;
}

/**
 * Load analytics data from precomputed JSON files or database
 * @returns {Promise<Object>} Analytics data
 */
export async function loadAnalytics() {
    const epoch = analyticsEpoch;

    if (analyticsData) {
        return analyticsData;
    }

    isLoading = true;
    renderLoadingState();

    try {
        // Try to load precomputed JSON files
        const loadedAnalytics = await loadPrecomputedAnalytics();
        if (!isCurrentAnalyticsEpoch(epoch)) {
            return null;
        }
        analyticsData = loadedAnalytics;
    } catch (error) {
        console.warn('[Stats] Precomputed analytics not available, using database fallback:', error.message);

        // Fall back to database queries
        if (isDatabaseReady()) {
            const computed = computeAnalyticsFromDatabase();
            if (!isCurrentAnalyticsEpoch(epoch)) {
                return null;
            }
            analyticsData = computed;
        } else {
            throw new Error('Database not ready and precomputed analytics not available');
        }
    }

    if (!isCurrentAnalyticsEpoch(epoch)) {
        return null;
    }

    isLoading = false;
    return analyticsData;
}

/**
 * Load precomputed analytics from JSON files
 * @returns {Promise<Object>} Analytics bundle
 */
async function loadPrecomputedAnalytics() {
    const files = [
        'statistics.json',
        'timeline.json',
        'agent_summary.json',
        'workspace_summary.json',
        'top_terms.json'
    ];

    const results = {};

    for (const file of files) {
        try {
            const response = await fetchWithTimeout(`./data/${file}`);
            if (!response.ok) {
                throw new Error(`Failed to load ${file}: ${response.status}`);
            }
            const key = file.replace('.json', '').replace(/_/g, '_');
            results[key] = await response.json();
        } catch (error) {
            // Try alternate path (root level)
            const response = await fetchWithTimeout(`./${file}`);
            if (!response.ok) {
                throw new Error(`Analytics file not found: ${file}`);
            }
            const key = file.replace('.json', '').replace(/_/g, '_');
            results[key] = await response.json();
        }
    }

    return {
        statistics: results.statistics,
        timeline: results.timeline,
        agentSummary: results.agent_summary,
        workspaceSummary: results.workspace_summary,
        topTerms: results.top_terms
    };
}

function createFetchTimeoutSignal(timeoutMs) {
    if (typeof AbortSignal !== 'undefined' && typeof AbortSignal.timeout === 'function') {
        return {
            signal: AbortSignal.timeout(timeoutMs),
            cancel() {},
        };
    }

    if (typeof AbortController === 'undefined') {
        return {
            signal: undefined,
            cancel() {},
        };
    }

    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), timeoutMs);
    return {
        signal: controller.signal,
        cancel() {
            clearTimeout(timeoutId);
        },
    };
}

async function fetchWithTimeout(url) {
    const { signal, cancel } = createFetchTimeoutSignal(ANALYTICS_FETCH_TIMEOUT_MS);
    try {
        return await fetch(url, signal ? { signal } : undefined);
    } finally {
        cancel();
    }
}

/**
 * Compute analytics from database (fallback)
 * @returns {Object} Analytics data
 */
function computeAnalyticsFromDatabase() {
    if (computedAnalytics) {
        return computedAnalytics;
    }

    // Statistics
    const totalConversations = queryValue('SELECT COUNT(*) FROM conversations') || 0;
    const totalMessages = queryValue('SELECT COUNT(*) FROM messages') || 0;
    const totalCharacters = queryValue('SELECT COALESCE(SUM(LENGTH(content)), 0) FROM messages') || 0;

    // Time range
    const timeRange = queryOne('SELECT MIN(started_at) as earliest, MAX(started_at) as latest FROM conversations');

    // Agent stats
    const agentRows = queryAll(`
        SELECT c.agent, COUNT(DISTINCT c.id) as conversations, COUNT(m.id) as messages
        FROM conversations c
        LEFT JOIN messages m ON c.id = m.conversation_id
        GROUP BY c.agent
        ORDER BY conversations DESC
    `);

    const agents = {};
    agentRows.forEach(row => {
        agents[row.agent] = {
            conversations: row.conversations,
            messages: row.messages
        };
    });

    // Role counts
    const roleRows = queryAll('SELECT role, COUNT(*) as count FROM messages GROUP BY role');
    const roles = {};
    roleRows.forEach(row => {
        roles[row.role] = row.count;
    });

    const statistics = {
        total_conversations: totalConversations,
        total_messages: totalMessages,
        total_characters: totalCharacters,
        agents: agents,
        roles: roles,
        time_range: {
            earliest: timeRange?.earliest ? new Date(timeRange.earliest).toISOString() : null,
            latest: timeRange?.latest ? new Date(timeRange.latest).toISOString() : null
        },
        computed_at: new Date().toISOString()
    };

    // Timeline (monthly aggregation for performance)
    const monthlyRows = queryAll(`
        SELECT strftime('%Y-%m', datetime(m.created_at/1000, 'unixepoch')) as month,
               COUNT(*) as messages,
               COUNT(DISTINCT m.conversation_id) as conversations
        FROM messages m
        WHERE m.created_at IS NOT NULL
        GROUP BY month
        ORDER BY month
    `);

    const timeline = {
        daily: [],
        weekly: [],
        monthly: monthlyRows.map(row => ({
            month: row.month,
            messages: row.messages,
            conversations: row.conversations
        })),
        by_agent: {}
    };

    // Agent summary
    const agentSummaryRows = queryAll(`
        SELECT c.agent as name,
               COUNT(DISTINCT c.id) as conversations,
               COUNT(m.id) as messages,
               MIN(c.started_at) as earliest,
               MAX(c.started_at) as latest
        FROM conversations c
        LEFT JOIN messages m ON c.id = m.conversation_id
        GROUP BY c.agent
        ORDER BY conversations DESC
    `);

    const agentSummary = {
        agents: agentSummaryRows.map(row => ({
            name: row.name,
            conversations: row.conversations,
            messages: row.messages,
            workspaces: [],
            date_range: {
                earliest: row.earliest ? new Date(row.earliest).toISOString() : null,
                latest: row.latest ? new Date(row.latest).toISOString() : null
            },
            avg_messages_per_conversation: row.conversations > 0 ? row.messages / row.conversations : 0
        }))
    };

    // Workspace summary
    const workspaceRows = queryAll(`
        SELECT c.workspace as path,
               COUNT(DISTINCT c.id) as conversations,
               COUNT(m.id) as messages,
               MIN(c.started_at) as earliest,
               MAX(c.started_at) as latest
        FROM conversations c
        LEFT JOIN messages m ON c.id = m.conversation_id
        WHERE c.workspace IS NOT NULL
        GROUP BY c.workspace
        ORDER BY conversations DESC
        LIMIT 50
    `);

    const workspaceSummary = {
        workspaces: workspaceRows.map(row => ({
            path: row.path,
            display_name: row.path ? row.path.split('/').pop() || row.path : 'Unknown',
            conversations: row.conversations,
            messages: row.messages,
            agents: [],
            date_range: {
                earliest: row.earliest ? new Date(row.earliest).toISOString() : null,
                latest: row.latest ? new Date(row.latest).toISOString() : null
            },
            recent_titles: []
        }))
    };

    // Top terms (simplified - extract from titles)
    const topTerms = {
        terms: []
    };

    try {
        const titleRows = queryAll('SELECT title FROM conversations WHERE title IS NOT NULL LIMIT 500');
        const termCounts = {};
        const stopWords = new Set(['the', 'a', 'an', 'and', 'or', 'but', 'in', 'on', 'at', 'to', 'for', 'of', 'with', 'by', 'from', 'is', 'it', 'as', 'was', 'be', 'are', 'been', 'have', 'has', 'had', 'do', 'does', 'did', 'will', 'would', 'could', 'should', 'this', 'that', 'these', 'those', 'i', 'you', 'we', 'they', 'what', 'which', 'who', 'when', 'where', 'why', 'how']);

        titleRows.forEach(row => {
            const title = typeof row.title === 'string'
                ? row.title
                : row.title === undefined || row.title === null
                    ? ''
                    : String(row.title);

            if (title) {
                const words = title.toLowerCase().split(/\s+/);
                words.forEach(word => {
                    const cleaned = word.replace(/[^a-z0-9_-]/g, '');
                    if (cleaned.length >= 3 && !stopWords.has(cleaned)) {
                        termCounts[cleaned] = (termCounts[cleaned] || 0) + 1;
                    }
                });
            }
        });

        topTerms.terms = Object.entries(termCounts)
            .sort((a, b) => (b[1] - a[1]) || (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0))
            .slice(0, 50);
    } catch (error) {
        console.warn('[Stats] Failed to compute top terms:', error);
    }

    computedAnalytics = {
        statistics,
        timeline,
        agentSummary,
        workspaceSummary,
        topTerms
    };

    return computedAnalytics;
}

/**
 * Render the stats dashboard
 */
export async function renderStatsDashboard() {
    if (!container) {
        console.error('[Stats] Container not set');
        return;
    }

    const epoch = analyticsEpoch;

    try {
        const data = await loadAnalytics();
        if (!data || !isCurrentAnalyticsEpoch(epoch)) {
            return;
        }
        renderDashboard(data);
    } catch (error) {
        if (!isCurrentAnalyticsEpoch(epoch)) {
            return;
        }
        console.error('[Stats] Failed to load analytics:', error);
        renderErrorState(error.message);
    }
}

/**
 * Render loading state
 */
function renderLoadingState() {
    if (!container) return;

    container.innerHTML = `
        <div class="panel stats-panel">
            <header class="panel-header">
                <h2>Archive Statistics</h2>
            </header>
            <div class="panel-content stats-loading">
                <div class="loading-spinner" aria-label="Loading statistics"></div>
                <p>Loading analytics data...</p>
            </div>
        </div>
    `;
}

/**
 * Render error state
 * @param {string} message - Error message
 */
function renderErrorState(message) {
    if (!container) return;

    container.innerHTML = `
        <div class="panel stats-panel">
            <header class="panel-header">
                <h2>Archive Statistics</h2>
            </header>
            <div class="panel-content stats-error">
                <div class="error-icon" aria-hidden="true">!</div>
                <p class="error-message">Failed to load statistics</p>
                <p class="error-details">${escapeHtml(message)}</p>
                <button type="button" class="btn btn-primary" id="stats-retry-btn">
                    Retry
                </button>
            </div>
        </div>
    `;

    document.getElementById('stats-retry-btn')?.addEventListener('click', () => {
        analyticsData = null;
        computedAnalytics = null;
        renderStatsDashboard();
    });
}

/**
 * Render the full dashboard
 * @param {Object} data - Analytics data
 */
function renderDashboard(data) {
    if (!container) return;

    const { statistics = {}, timeline = {}, agentSummary, workspaceSummary, topTerms } = data || {};
    const statisticsAgents = isPlainObject(statistics.agents) ? statistics.agents : {};
    const statisticsRoles = isPlainObject(statistics.roles) ? statistics.roles : {};
    const agents = Array.isArray(agentSummary?.agents) ? agentSummary.agents : [];
    const workspaces = Array.isArray(workspaceSummary?.workspaces) ? workspaceSummary.workspaces : [];
    const terms = Array.isArray(topTerms?.terms) ? topTerms.terms : [];
    const availableTimelineViews = getAvailableTimelineViews(timeline);
    const selectedTimelineView = getSelectedTimelineView(timeline);
    currentTimelineView = selectedTimelineView;

    container.innerHTML = `
        <div class="panel stats-panel">
            <header class="panel-header">
                <h2>Archive Statistics</h2>
                ${statistics.computed_at ? `<span class="stats-timestamp">Updated ${formatRelativeTime(statistics.computed_at)}</span>` : ''}
            </header>
            <div class="panel-content">
                <!-- Overview Cards -->
                <section class="stats-section" aria-labelledby="overview-heading">
                    <h3 id="overview-heading" class="visually-hidden">Overview</h3>
                    <div class="stats-grid" role="list">
                        ${renderOverviewCard('Conversations', statistics.total_conversations, 'conversation-count')}
                        ${renderOverviewCard('Messages', statistics.total_messages, 'message-count')}
                        ${renderOverviewCard('Characters', formatNumber(statistics.total_characters), 'character-count')}
                        ${renderOverviewCard('Agents', Object.keys(statisticsAgents).length, 'agent-count')}
                    </div>
                </section>

                <!-- Time Range -->
                ${statistics.time_range?.earliest ? `
                    <section class="stats-section stats-time-range" aria-labelledby="timerange-heading">
                        <h3 id="timerange-heading">Time Range</h3>
                        <div class="time-range-display">
                            <span class="time-range-item">
                                <span class="time-range-label">From</span>
                                <span class="time-range-value">${formatDate(statistics.time_range.earliest)}</span>
                            </span>
                            <span class="time-range-separator" aria-hidden="true">&rarr;</span>
                            <span class="time-range-item">
                                <span class="time-range-label">To</span>
                                <span class="time-range-value">${formatDate(statistics.time_range.latest)}</span>
                            </span>
                            ${renderTimeSpan(statistics.time_range)}
                        </div>
                    </section>
                ` : ''}

                <!-- Timeline Sparkline -->
                ${availableTimelineViews.length > 0 ? `
                    <section class="stats-section stats-timeline" aria-labelledby="timeline-heading">
                        <h3 id="timeline-heading">Activity Timeline</h3>
                        ${availableTimelineViews.length > 1 ? `
                            <div class="timeline-controls" role="tablist" aria-label="Timeline view">
                                ${availableTimelineViews.map((view) => `
                                    <button type="button" role="tab" class="timeline-tab ${selectedTimelineView === view ? 'active' : ''}"
                                            data-view="${view}" aria-selected="${selectedTimelineView === view}">${formatTimelineViewLabel(view)}</button>
                                `).join('')}
                            </div>
                        ` : ''}
                        <div id="timeline-chart" class="timeline-chart" role="img" aria-label="Activity timeline chart">
                            ${renderTimelineChart(timeline, selectedTimelineView)}
                        </div>
                    </section>
                ` : ''}

                <!-- Agent Breakdown -->
                ${agents.length > 0 ? `
                    <section class="stats-section stats-agents" aria-labelledby="agents-heading">
                        <h3 id="agents-heading">Agents</h3>
                        <div class="stats-table-wrapper">
                            <table class="stats-table" aria-describedby="agents-heading">
                                <thead>
                                    <tr>
                                        <th scope="col">Agent</th>
                                        <th scope="col" class="numeric">Conversations</th>
                                        <th scope="col" class="numeric">Messages</th>
                                        <th scope="col" class="numeric">Avg/Conv</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    ${agents.map(agent => `
                                        <tr>
                                            <td>
                                                <span class="agent-badge agent-${toCssSlug(agent.name)}">
                                                    ${escapeHtml(formatAgentName(agent.name))}
                                                </span>
                                            </td>
                                            <td class="numeric">${formatNumber(agent.conversations)}</td>
                                            <td class="numeric">${formatNumber(agent.messages)}</td>
                                            <td class="numeric">${formatDecimal(agent.avg_messages_per_conversation, 1, '-')}</td>
                                        </tr>
                                    `).join('')}
                                </tbody>
                            </table>
                        </div>
                    </section>
                ` : ''}

                <!-- Workspace Breakdown -->
                ${workspaces.length > 0 ? `
                    <section class="stats-section stats-workspaces" aria-labelledby="workspaces-heading">
                        <h3 id="workspaces-heading">Top Workspaces</h3>
                        <div class="stats-table-wrapper">
                            <table class="stats-table" aria-describedby="workspaces-heading">
                                <thead>
                                    <tr>
                                        <th scope="col">Workspace</th>
                                        <th scope="col" class="numeric">Conversations</th>
                                        <th scope="col" class="numeric">Messages</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    ${workspaces.slice(0, 10).map(ws => `
                                        <tr>
                                            <td>
                                                <span class="workspace-name" title="${escapeAttribute(ws.path)}">
                                                    ${escapeHtml(ws.display_name)}
                                                </span>
                                            </td>
                                            <td class="numeric">${formatNumber(ws.conversations)}</td>
                                            <td class="numeric">${formatNumber(ws.messages)}</td>
                                        </tr>
                                    `).join('')}
                                </tbody>
                            </table>
                            ${workspaces.length > 10 ? `
                                <p class="stats-more">... and ${workspaces.length - 10} more workspaces</p>
                            ` : ''}
                        </div>
                    </section>
                ` : ''}

                <!-- Top Terms -->
                ${terms.length > 0 ? `
                    <section class="stats-section stats-terms" aria-labelledby="terms-heading">
                        <h3 id="terms-heading">Top Topics</h3>
                        <div class="terms-cloud" role="list" aria-label="Topic frequency">
                            ${renderTermsCloud(terms)}
                        </div>
                    </section>
                ` : ''}

                <!-- Role Distribution -->
                ${Object.keys(statisticsRoles).length > 0 ? `
                    <section class="stats-section stats-roles" aria-labelledby="roles-heading">
                        <h3 id="roles-heading">Message Roles</h3>
                        <div class="role-bars">
                            ${renderRoleBars(statisticsRoles)}
                        </div>
                    </section>
                ` : ''}
            </div>
        </div>
    `;

    applyDynamicStatsStyles();

    // Set up timeline tab handlers
    setupTimelineControls(timeline);
}

/**
 * Render an overview card
 * @param {string} label - Card label
 * @param {number|string} value - Card value
 * @param {string} id - Unique ID for the card
 * @returns {string} HTML string
 */
function renderOverviewCard(label, value, id) {
    const displayValue = typeof value === 'number'
        ? formatNumber(value)
        : escapeHtml(value);
    return `
        <div class="stat-card" role="listitem">
            <div class="stat-card-value" id="${escapeAttribute(id)}">${displayValue}</div>
            <div class="stat-card-label">${escapeHtml(label)}</div>
        </div>
    `;
}

/**
 * Render time span badge
 * @param {Object} timeRange - Time range object
 * @returns {string} HTML string
 */
function renderTimeSpan(timeRange) {
    if (!timeRange.earliest || !timeRange.latest) return '';

    const earliest = parseValidDate(timeRange.earliest);
    const latest = parseValidDate(timeRange.latest);
    if (!earliest || !latest) return '';

    const days = Math.ceil((latest - earliest) / (1000 * 60 * 60 * 24));

    if (days === 0) return '<span class="time-span-badge">Same day</span>';
    if (days === 1) return '<span class="time-span-badge">1 day</span>';
    if (days < 30) return `<span class="time-span-badge">${days} days</span>`;
    if (days < 365) return `<span class="time-span-badge">${Math.round(days / 30)} months</span>`;
    return `<span class="time-span-badge">${(days / 365).toFixed(1)} years</span>`;
}

/**
 * Get timeline entries for a specific view
 * @param {Object} timeline - Timeline data
 * @param {string} view - Timeline view key
 * @returns {Array} Timeline entries
 */
function getTimelineEntries(timeline, view) {
    if (!timeline || !Array.isArray(timeline[view])) {
        return [];
    }
    return timeline[view];
}

function getAvailableTimelineViews(timeline) {
    return ['daily', 'weekly', 'monthly'].filter((view) => getTimelineEntries(timeline, view).length > 0);
}

function getSelectedTimelineView(timeline) {
    const availableViews = getAvailableTimelineViews(timeline);
    if (availableViews.includes(currentTimelineView)) {
        return currentTimelineView;
    }
    return availableViews[0] || 'monthly';
}

function formatTimelineViewLabel(view) {
    if (typeof view !== 'string' || view.length === 0) {
        return 'Timeline';
    }
    return view.charAt(0).toUpperCase() + view.slice(1);
}

/**
 * Render timeline chart (SVG sparkline)
 * @param {Object} timeline - Timeline data
 * @param {string} view - Timeline view
 * @returns {string} SVG HTML string
 */
function renderTimelineChart(timeline, view = currentTimelineView) {
    const data = getTimelineEntries(timeline, view).map((entry) => ({
        ...entry,
        messages: toNonNegativeNumber(entry?.messages),
        conversations: toNonNegativeNumber(entry?.conversations),
    }));
    if (data.length === 0) {
        return '<p class="no-data">No timeline data available</p>';
    }

    const width = 600;
    const height = 120;
    const padding = 20;
    const chartWidth = width - padding * 2;
    const chartHeight = height - padding * 2;

    const maxMessages = Math.max(...data.map(d => d.messages));
    if (maxMessages === 0) {
        return '<p class="no-data">No activity data</p>';
    }

    const barWidth = Math.max(2, Math.min(20, chartWidth / data.length - 2));
    const barSpacing = (chartWidth - barWidth * data.length) / (data.length - 1 || 1);

    const bars = data.map((d, i) => {
        const barHeight = (d.messages / maxMessages) * chartHeight;
        const x = padding + i * (barWidth + barSpacing);
        const y = padding + chartHeight - barHeight;

        const label = getTimelineLabel(d);
        const ariaLabel = `${label}: ${formatNumber(d.messages)} messages`;
        const title = `${label}: ${formatNumber(d.messages)} messages, ${formatNumber(d.conversations)} conversations`;

        return `
            <rect x="${x}" y="${y}" width="${barWidth}" height="${barHeight}"
                  class="timeline-bar" data-messages="${d.messages}" data-conversations="${d.conversations}"
                  aria-label="${escapeAttribute(ariaLabel)}">
                <title>${escapeHtml(title)}</title>
            </rect>
        `;
    }).join('');

    // X-axis labels (first, middle, last)
    const labels = [];
    if (data.length > 0) {
        labels.push({ x: padding, label: getTimelineLabel(data[0]) });
        if (data.length > 2) {
            const midIdx = Math.floor(data.length / 2);
            labels.push({ x: padding + midIdx * (barWidth + barSpacing), label: getTimelineLabel(data[midIdx]) });
        }
        if (data.length > 1) {
            labels.push({ x: padding + (data.length - 1) * (barWidth + barSpacing), label: getTimelineLabel(data[data.length - 1]) });
        }
    }

    const axisLabels = labels.map(l => `
        <text x="${l.x}" y="${height - 2}" class="timeline-label">${escapeHtml(l.label)}</text>
    `).join('');

    return `
        <svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="xMidYMid meet" class="timeline-svg"
             role="img" aria-label="Activity over time">
            ${bars}
            ${axisLabels}
        </svg>
    `;
}

/**
 * Get timeline label from data point
 * @param {Object} d - Data point
 * @returns {string} Label
 */
function getTimelineLabel(d) {
    if (d?.date !== undefined && d.date !== null) return String(d.date);
    if (d?.week !== undefined && d.week !== null) return String(d.week);
    if (d?.month !== undefined && d.month !== null) return String(d.month);
    return '';
}

/**
 * Render terms cloud
 * @param {Array} terms - Array of [term, count] tuples
 * @returns {string} HTML string
 */
function renderTermsCloud(terms) {
    const normalizedTerms = terms
        .slice(0, 30)
        .map((termEntry) => {
            if (Array.isArray(termEntry)) {
                return [termEntry[0], toNonNegativeNumber(termEntry[1])];
            }
            return [termEntry, 0];
        })
        .filter(([term]) => term !== undefined && term !== null && String(term).length > 0);

    if (normalizedTerms.length === 0) {
        return '';
    }

    const maxCount = Math.max(...normalizedTerms.map(t => t[1]));
    const minCount = Math.min(...normalizedTerms.map(t => t[1]));
    const range = maxCount - minCount || 1;

    return normalizedTerms.map(([term, count]) => {
        const size = 0.8 + ((count - minCount) / range) * 0.6; // 0.8em to 1.4em
        const opacity = 0.6 + ((count - minCount) / range) * 0.4; // 0.6 to 1.0

        return `
            <span class="term-tag" role="listitem"
                  data-term-size="${size.toFixed(3)}"
                  data-term-opacity="${opacity.toFixed(3)}"
                  title="${escapeAttribute(`${formatNumber(count)} occurrences`)}">
                ${escapeHtml(term)}
            </span>
        `;
    }).join('');
}

/**
 * Render role distribution bars
 * @param {Object} roles - Role counts
 * @returns {string} HTML string
 */
function renderRoleBars(roles) {
    const roleEntries = Object.entries(roles)
        .map(([role, count]) => [role, toNonNegativeNumber(count)]);
    const total = roleEntries.reduce((sum, [, count]) => sum + count, 0);
    if (total === 0) return '';

    return roleEntries
        .sort((a, b) => b[1] - a[1])
        .map(([role, count]) => {
            const percent = (count / total * 100).toFixed(1);
            return `
                <div class="role-bar-item">
                    <span class="role-name">${escapeHtml(role)}</span>
                    <div class="role-bar-container">
                        <div class="role-bar role-${toCssSlug(role)}" data-role-width="${percent}"
                             aria-valuenow="${percent}" aria-valuemin="0" aria-valuemax="100"></div>
                    </div>
                    <span class="role-count">${formatNumber(count)} (${percent}%)</span>
                </div>
            `;
        }).join('');
}

function applyDynamicStatsStyles() {
    if (!container) {
        return;
    }

    container.querySelectorAll('[data-term-size]').forEach(term => {
        const fontSize = Number.parseFloat(term.dataset.termSize || '');
        const opacity = Number.parseFloat(term.dataset.termOpacity || '');

        if (Number.isFinite(fontSize)) {
            term.style.fontSize = `${Math.min(Math.max(fontSize, 0.8), 1.4)}em`;
        }
        if (Number.isFinite(opacity)) {
            term.style.opacity = String(Math.min(Math.max(opacity, 0.6), 1));
        }
    });

    container.querySelectorAll('[data-role-width]').forEach(roleBar => {
        const width = Number.parseFloat(roleBar.dataset.roleWidth || '');
        if (Number.isFinite(width)) {
            roleBar.style.width = `${Math.min(Math.max(width, 0), 100)}%`;
        }
    });
}

/**
 * Set up timeline control event handlers
 * @param {Object} timeline - Timeline data
 */
function setupTimelineControls(timeline) {
    const tabs = container.querySelectorAll('.timeline-tab');
    const availableViews = new Set(getAvailableTimelineViews(timeline));
    tabs.forEach(tab => {
        tab.addEventListener('click', () => {
            const view = tab.dataset.view;
            if (view && availableViews.has(view)) {
                currentTimelineView = view;

                // Update tab states
                tabs.forEach(t => {
                    t.classList.toggle('active', t.dataset.view === view);
                    t.setAttribute('aria-selected', t.dataset.view === view);
                });

                // Re-render chart
                const chartContainer = container?.querySelector('#timeline-chart');
                if (chartContainer) {
                    chartContainer.innerHTML = renderTimelineChart(timeline, view);
                }
            }
        });
    });
}

/**
 * Format agent name for display
 * @param {string} agent - Agent identifier
 * @returns {string} Formatted name
 */
function formatAgentName(agent) {
    if (agent === undefined || agent === null || agent === '') return 'Unknown';
    const value = String(agent);
    return value.charAt(0).toUpperCase() + value.slice(1).replace(/[-_]/g, ' ');
}

function toCssSlug(value, fallback = 'unknown') {
    if (value === undefined || value === null || value === '') {
        return fallback;
    }

    const slug = String(value).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
    return slug || fallback;
}

/**
 * Format date for display
 * @param {string} timestamp - ISO timestamp
 * @returns {string} Formatted date
 */
function formatDate(timestamp) {
    if (!timestamp) return 'Unknown';

    const date = parseValidDate(timestamp);
    if (!date) return 'Unknown';

    return date.toLocaleDateString(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric'
    });
}

/**
 * Format relative time
 * @param {string} timestamp - ISO timestamp
 * @returns {string} Relative time string
 */
function formatRelativeTime(timestamp) {
    if (!timestamp) return '';

    const date = parseValidDate(timestamp);
    if (!date) return '';

    const now = new Date();
    const diff = now - date;

    const minutes = Math.floor(diff / 60000);
    if (minutes < 1) return 'just now';
    if (minutes < 60) return `${minutes}m ago`;

    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;

    const days = Math.floor(hours / 24);
    if (days < 7) return `${days}d ago`;

    return formatDate(timestamp);
}

/**
 * Format number with thousands separators
 * @param {number} num - Number to format
 * @returns {string} Formatted number
 */
function formatNumber(num) {
    return toFiniteNumber(num).toLocaleString();
}

function formatDecimal(value, digits = 1, fallback = '-') {
    const number = Number(value);
    if (!Number.isFinite(number)) {
        return fallback;
    }
    return number.toFixed(digits);
}

function toFiniteNumber(value, fallback = 0) {
    const number = Number(value);
    return Number.isFinite(number) ? number : fallback;
}

function toNonNegativeNumber(value, fallback = 0) {
    return Math.max(0, toFiniteNumber(value, fallback));
}

function isPlainObject(value) {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function parseValidDate(timestamp) {
    const date = new Date(timestamp);
    return Number.isFinite(date.getTime()) ? date : null;
}

/**
 * Escape HTML special characters
 * @param {string} text - Text to escape
 * @returns {string} Escaped text
 */
function escapeHtml(text) {
    if (text === undefined || text === null) return '';
    const div = document.createElement('div');
    div.textContent = String(text);
    return div.innerHTML;
}

function escapeAttribute(text) {
    return escapeHtml(text)
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

/**
 * Clear cached analytics data
 */
export function clearStatsCache() {
    analyticsEpoch += 1;
    analyticsData = null;
    computedAnalytics = null;
    isLoading = false;
    currentTimelineView = 'monthly';
    if (container) {
        container.innerHTML = '';
    }
}

/**
 * Get current analytics data (if loaded)
 * @returns {Object|null} Analytics data or null
 */
export function getAnalyticsData() {
    return analyticsData;
}

// Export default
export default {
    initStats,
    loadAnalytics,
    renderStatsDashboard,
    clearStatsCache,
    getAnalyticsData
};
