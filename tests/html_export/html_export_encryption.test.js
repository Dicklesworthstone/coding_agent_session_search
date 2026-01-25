/**
 * HTML Export Encryption E2E (WebCrypto)
 *
 * Run:
 *   npx playwright test tests/html_export/html_export_encryption.test.js
 *
 * This test spins up a tiny HTTP server to serve an encrypted HTML export
 * and validates client-side WebCrypto decryption behavior with rich logging.
 */

import { test, expect } from '@playwright/test';
import crypto from 'crypto';
import http from 'http';
import { TextEncoder } from 'util';

const TEST_PASSWORD = 'correct-horse-battery-staple';
const WRONG_PASSWORD = 'totally-wrong-password';
const PLAINTEXT_HTML =
  '<div class="message-content">Hello from encrypted export ✅</div>';
const ITERATIONS = 1000; // Lower for test speed; payload carries this value.

let server;
let baseURL;

function logEvent(event) {
  console.log(JSON.stringify({ event, ts: new Date().toISOString() }));
}

function escapeHtml(value) {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function toBase64(bytes) {
  return Buffer.from(bytes).toString('base64');
}

async function encryptPayload() {
  const encoder = new TextEncoder();
  const salt = Uint8Array.from([
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
  ]);
  const iv = Uint8Array.from([15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4]);

  const webcrypto = crypto.webcrypto;
  if (!webcrypto?.subtle) {
    throw new Error('WebCrypto subtle API not available in Node');
  }

  const keyMaterial = await webcrypto.subtle.importKey(
    'raw',
    encoder.encode(TEST_PASSWORD),
    'PBKDF2',
    false,
    ['deriveKey']
  );

  const key = await webcrypto.subtle.deriveKey(
    {
      name: 'PBKDF2',
      salt,
      iterations: ITERATIONS,
      hash: 'SHA-256',
    },
    keyMaterial,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt']
  );

  const ciphertext = await webcrypto.subtle.encrypt(
    { name: 'AES-GCM', iv },
    key,
    encoder.encode(PLAINTEXT_HTML)
  );

  return {
    salt: toBase64(salt),
    iv: toBase64(iv),
    ciphertext: toBase64(new Uint8Array(ciphertext)),
    iterations: ITERATIONS,
  };
}

function buildHtml(payload) {
  const payloadJson = escapeHtml(JSON.stringify(payload));
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Encrypted HTML Export Test</title>
  <style>
    body { font-family: sans-serif; padding: 2rem; }
    .modal { position: fixed; inset: 0; display: grid; place-items: center; background: rgba(0,0,0,0.4); }
    .modal-content { background: #111827; color: #fff; padding: 1.5rem; border-radius: 12px; width: 360px; }
    .modal-input { width: 100%; padding: 0.5rem; margin-top: 0.5rem; }
    .modal-btn { margin-top: 0.75rem; width: 100%; padding: 0.5rem; }
    .modal-error { margin-top: 0.75rem; color: #f87171; }
  </style>
</head>
<body>
  <main id="conversation">
    <div id="encrypted-content" hidden>${payloadJson}</div>
    <div class="encrypted-notice">Encrypted export loaded.</div>
  </main>

  <div id="password-modal" class="modal" role="dialog" aria-modal="true">
    <div class="modal-content">
      <h2 id="modal-title">Enter Password</h2>
      <p>Enter the password to decrypt this export.</p>
      <form id="password-form">
        <input id="password-input" class="modal-input" type="password" />
        <button type="submit" class="modal-btn">Decrypt</button>
      </form>
      <p id="decrypt-error" class="modal-error" hidden></p>
    </div>
  </div>

  <script>
    const $ = (sel) => document.querySelector(sel);

    const Crypto = {
      modal: null,
      form: null,
      errorEl: null,

      init() {
        this.modal = $('#password-modal');
        this.form = $('#password-form');
        this.errorEl = $('#decrypt-error');

        if (!this.modal || !this.form) return;

        this.form.addEventListener('submit', (e) => {
          e.preventDefault();
          this.decrypt();
        });
      },

      async decrypt() {
        const password = $('#password-input').value;
        if (!password) return;

        try {
          this.errorEl.hidden = true;

          const encryptedEl = $('#encrypted-content');
          if (!encryptedEl) throw new Error('No encrypted content found');

          const encryptedData = JSON.parse(encryptedEl.textContent);
          const { salt, iv, ciphertext, iterations } = encryptedData;
          if (!salt || !iv || !ciphertext || !Number.isInteger(iterations) || iterations <= 0) {
            throw new Error('Invalid encryption parameters');
          }

          const enc = new TextEncoder();
          const keyMaterial = await crypto.subtle.importKey(
            'raw',
            enc.encode(password),
            'PBKDF2',
            false,
            ['deriveBits', 'deriveKey']
          );

          const key = await crypto.subtle.deriveKey(
            {
              name: 'PBKDF2',
              salt: this.base64ToBytes(salt),
              iterations: iterations,
              hash: 'SHA-256',
            },
            keyMaterial,
            { name: 'AES-GCM', length: 256 },
            false,
            ['decrypt']
          );

          const decrypted = await crypto.subtle.decrypt(
            {
              name: 'AES-GCM',
              iv: this.base64ToBytes(iv),
            },
            key,
            this.base64ToBytes(ciphertext)
          );

          const dec = new TextDecoder();
          const plaintext = dec.decode(decrypted);
          const conversation = $('#conversation');
          conversation.innerHTML = plaintext;

          this.modal.hidden = true;
          this.form.reset();
        } catch (e) {
          this.errorEl.textContent = 'Decryption failed. Wrong password?';
          this.errorEl.hidden = false;
        }
      },

      base64ToBytes(base64) {
        const binary = atob(base64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        return bytes;
      },
    };

    document.addEventListener('DOMContentLoaded', () => {
      Crypto.init();
    });
  </script>
</body>
</html>`;
}

test.beforeAll(async () => {
  const start = Date.now();
  const payload = await encryptPayload();
  logEvent({ phase: 'payload_generated', ms: Date.now() - start });

  const html = buildHtml(payload);

  server = http.createServer((req, res) => {
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Cache-Control': 'no-store',
    });
    res.end(html);
  });

  await new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      baseURL = `http://127.0.0.1:${port}/`;
      logEvent({ phase: 'server_listening', port });
      resolve();
    });
  });
});

test.afterAll(async () => {
  if (!server) return;
  await new Promise((resolve) => server.close(resolve));
  logEvent({ phase: 'server_closed' });
});

test.beforeEach(async ({ page }, testInfo) => {
  const browser = testInfo.project.name || 'default';
  page.on('pageerror', (err) => {
    logEvent({ phase: 'page_error', browser, message: err.message });
  });
  page.on('console', (msg) => {
    logEvent({
      phase: 'browser_console',
      browser,
      level: msg.type(),
      text: msg.text().slice(0, 300),
    });
  });
});

test('decrypts with correct password', async ({ page }, testInfo) => {
  const navStart = Date.now();
  await page.goto(baseURL, { waitUntil: 'domcontentloaded' });
  logEvent({
    phase: 'page_loaded',
    browser: testInfo.project.name || 'default',
    ms: Date.now() - navStart,
  });

  const webcryptoAvailable = await page.evaluate(() => !!crypto?.subtle);
  expect(webcryptoAvailable).toBe(true);

  await page.fill('#password-input', TEST_PASSWORD);
  await page.click('#password-form button[type="submit"]');

  await expect(page.locator('#conversation .message-content')).toHaveText(
    'Hello from encrypted export ✅'
  );
  await expect(page.locator('#password-modal')).toBeHidden();
});

test('shows error on wrong password', async ({ page }) => {
  await page.goto(baseURL, { waitUntil: 'domcontentloaded' });

  await page.fill('#password-input', WRONG_PASSWORD);
  await page.click('#password-form button[type="submit"]');

  await expect(page.locator('#decrypt-error')).toBeVisible();
  await expect(page.locator('#decrypt-error')).toContainText('Decryption failed');
});
