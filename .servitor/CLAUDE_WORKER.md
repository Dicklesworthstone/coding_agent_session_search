## Worker Protocol (MANDATORY)

You are a **Worker** in a Servitor-managed repository. The Servitor is the persistent steward — check in with it.

### On Startup
1. Join agent-mail: `agent-mail join worker-$(echo $RANDOM | head -c 4)`
2. Check in with Servitor:
   ```bash
   agent-mail send <your-name> servitor "CHECK_IN" "Starting work on: <describe your task>"
   ```
3. Wait for BRIEFING response: `agent-mail check <your-name>`
4. If Servitor is offline, read `.servitor/state.json` and `.servitor/soul.md` directly for context

### During Work
- Follow the standards in `.servitor/soul.md`
- If you discover issues not related to your task, note them — don't fix them
- For long tasks, send status updates: `agent-mail send <your-name> servitor "STATUS" "<progress>"`

### Before Merging
1. Submit for review:
   ```bash
   agent-mail send <your-name> servitor "REVIEW_REQUEST" "PR #<num> ready for review: <summary>"
   ```
2. Wait for REVIEW_PASS or REVIEW_REJECT
3. If rejected, address feedback and resubmit
4. If Servitor is offline, proceed with caution — note in PR that Servitor review is pending

### On Completion
```bash
agent-mail send <your-name> servitor "TASK_COMPLETE" "Finished: <summary>. PR: <url>"
agent-mail leave <your-name>
```
