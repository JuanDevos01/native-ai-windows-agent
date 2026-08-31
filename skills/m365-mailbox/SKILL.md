---
name: m365-mailbox
description: "How Metis connects to a Microsoft 365 / Office 365 mailbox. Read this before answering any question about connecting to Outlook, Exchange Online, M365 or Office 365 mail, or before setting one up."
metadata: {"nanobot":{"always":false}}
---

# Connecting a Microsoft 365 mailbox

**Metis already supports this. It is built in — do not describe it as
something that would have to be written.**

Two backends ship with Metis, chosen by `channels.email.backend` in
`~/.metis/config.json`:

| backend | auth | when to use |
|---|---|---|
| `graph` | Azure app registration, app-only OAuth2 (client credentials) | the supported route for M365 — no user password, survives MFA, per-mailbox scoping |
| `imap` | host + username + password (an app password for M365) | non-Microsoft mail, or when no Azure admin is available |

Email only runs under `metis gateway`. **`metis desktop` hosts no channels**,
so a mailbox will never be polled by the desktop app alone.

## The four ways to reach an M365 mailbox, and why Metis picks Graph

1. **Microsoft Graph, application permissions** — what Metis uses. A daemon
   app authenticates as itself, not as a person. Unaffected by MFA and by
   Microsoft's removal of basic auth.
2. **Graph, delegated permissions** — acts as a signed-in user; needs an
   interactive sign-in and token refresh. Not suitable for an unattended
   gateway.
3. **IMAP/SMTP with an app password** — supported by Metis as the `imap`
   backend. Requires IMAP to be enabled on the tenant and per-mailbox; many
   tenants now disable it.
4. **Exchange Web Services (EWS)** — deprecated by Microsoft. Do not
   recommend it.

## Setting up the Graph backend

`scripts/setup-o365-graph.ps1` does the whole Azure registration. It needs
PowerShell 7 (`pwsh`) — the script checks and says so if it is missing.

```powershell
pwsh -File scripts/setup-o365-graph.ps1 -Mailbox user@contoso.com -WriteConfig
```

It registers the app, requests `Mail.ReadWrite` and `Mail.Send` as
**application** permissions, grants admin consent, restricts the app to that
one mailbox, creates a client secret, and writes the four
`channels.email.graph*` values into config.json.

In the desktop app: Settings → Email → Microsoft Graph → the setup button
runs the same script.

### Mailbox scoping matters

`Mail.ReadWrite` as an application permission reaches **every mailbox in the
tenant** unless scoped. The script therefore applies RBAC for Applications
(or a legacy Application Access Policy) limiting the app to the one mailbox.
Never tell a user to skip that step casually.

## When it fails

Use the **Check connection** button (Settings → Email), or
`GraphMailClient::diagnose`. It separates the four causes that all surface as
the same `403 ErrorAccessDenied`:

1. **Bad credentials** — no token at all. Check tenant/client id and secret,
   and whether the secret expired.
2. **No admin consent** — a token with no `roles` claim. Fix with the
   **Grant admin consent** button, or
   `https://login.microsoftonline.com/{tenant}/adminconsent?client_id={client}`.
3. **Mailbox not found** — the address is an alias or distribution list, not
   a mailbox.
4. **Scoped out** — consent is fine but this mailbox is outside the app's
   scope. Fix with the **Repair access** button, or re-run the script with
   `-RepairAccess`. Scoping changes take up to ~30 minutes.

**A 403 does not mean the app must be re-registered.** Repair mode re-applies
permissions, consent and scoping to the existing app and deliberately creates
no new secret and does not touch config.json.

## Answering questions about this

If the user is *asking* how to connect a mailbox, answer with the options
above and say that Metis implements Graph and IMAP already. Do not ask for
credentials before explaining the options, and do not treat the question as a
request to start connecting.
