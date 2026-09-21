import { existsSync, readFileSync } from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

function readEnvFile(envPath: string): Record<string, string> {
  if (!existsSync(envPath)) {
    return {};
  }
  const envContent = readFileSync(envPath, "utf-8");
  const env: Record<string, string> = {};
  for (const rawLine of envContent.split("\n")) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) {
      continue;
    }
    const [key, ...valueParts] = line.split("=");
    if (key && valueParts.length > 0) {
      env[key] = valueParts.join("=");
    }
  }
  return env;
}

async function globalTeardown() {
  const envPath = path.resolve(__dirname, "../.env.test");
  const env = readEnvFile(envPath);
  const pidRaw = env.TEST_PAGES_PREVIEW_PID;
  if (!pidRaw) {
    return;
  }
  const pid = Number(pidRaw);
  if (!Number.isFinite(pid) || pid <= 0) {
    return;
  }
  try {
    process.kill(pid, "SIGTERM");
  } catch (err) {
    // Ignore if already stopped.
  }
}

export default globalTeardown;
