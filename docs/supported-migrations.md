# Supported migrations

What `ecm` can copy or repair, by item type and by path. Updated with every change
to what can be migrated. Milestones: [plan.md](plan.md#7-milestones).

✅ supported · 🟡 partly · ⏳ planned · ✗ not possible or not planned

## Sources and targets

| | As a source | As a target |
|---|---|---|
| **PST** (Outlook data file) | ✅ read-only[^1] | ✗ never |
| **EWS** (Exchange on-premises or hosted) | ⏳ M7 | ✅ |
| **Graph** (Outlook.com, Microsoft 365)[^2] | ✅ | ✅ messages; ⏳ M9b the rest |

## Matrix

| Item type | PST → EWS | PST → Graph | Graph → EWS | Graph → Graph | EWS → EWS | EWS → Graph |
|---|---|---|---|---|---|---|
| Received messages | ✅ `ecm plan-pst` | ✅ `ecm plan-pst --outlook-com`[^3] | ✅ `ecm plan` | ✗ | ⏳ M7 | ⏳ M7 |
| Sent messages | ✅[^4] | ✅[^4] | 🟡[^5] | ✗ | ⏳ M7 | ⏳ M7 |
| Large messages | ✅[^6] | 🟡[^7] | 🟡[^8] | ✗ | ⏳ M7 | ⏳ M7 |
| Damaged copies, replaced and quarantined | ✅[^9] | 🟡[^10] | ✅ `ecm plan --after` | ✗ | ⏳ M7 | ⏳ M7 |
| Folders | ✅ | ✅ | ✅ | ✗ | ⏳ M7 | ⏳ M7 |
| Calendar | ⏳ M10 | ⏳ M10 | 🟡 `ecm plan-pim`[^11] | ✗ | ⏳ M7 | ⏳ M9b |
| Contacts | ⏳ M10 | ⏳ M10 | ✅ `ecm plan-pim` | ✗ | ⏳ M7 | ⏳ M9b |
| Contact photos | ⏳ M10 | ⏳ M10 | ✅ `ecm plan-contact-photos` | ✗ | ⏳ M7 | ⏳ M9b |
| Tasks | ⏳ M10 | ⏳ M10 | ✅ `ecm plan-pim`[^12] | ✗ | ⏳ M7 | ⏳ M9b |
| Sticky notes | ✅ `ecm plan-notes` | ⏳ M9b[^13] | ✗[^13] | ✗ | ⏳ M7 | ⏳ M9b[^13] |
| Inbox rules | ✗[^14] | ✗[^14] | 🟡 `ecm plan-rules`[^15] | ✗ | ⏳ M7 | ⏳ M9b |
| Categories | ✗ | ✗ | ✗ | ✗ | ⏳ M7 | ⏳ M9b |
| Contact groups | ✗ | ✗ | ✗ | ✗ | ⏳ M7 | ⏳ M9b[^16] |
| Safe and blocked senders | ✗[^17] | ✗[^17] | ✗[^17] | ✗[^17] | ✗[^17] | ✗[^17] |

Plans run with `ecm apply` (EWS) or `ecm apply-graph` (Graph), journaled and
resumable. Nothing is ever deleted. Graph → Graph is not planned.

## Checks

| | PST | Graph | EWS |
|---|---|---|---|
| Inventory | ✅ `ecm inventory --pst` | ✅ `ecm inventory --outlook-com` | ✅ `ecm inventory --ews` |
| Audit, verify | ✅ `ecm audit`, `ecm verify` | ✅ `ecm audit --source outlook` | ⏳ M7 |
| Bodies and files against the PST | — | ✅ `ecm check-content --outlook-com` | ✅ `ecm check-content --ews` |
| Trial in `ecm-test` | — | ✅ `ecm fidelity-graph` | ✅ `ecm fidelity-test` |

[^1]: Opened read-only, checksum checked before and after.
[^2]: Tested on Outlook.com with personal Microsoft accounts. Microsoft 365 uses the
    same API but is untested: it needs an app registration that accepts work or
    school accounts, and `--tenant organizations` or a tenant id.
[^3]: Run with `ecm apply-graph`.
[^4]: A sent item without a sender address gets `--sender`.
[^5]: Outlook.com MIME can name the sender `IMCEAEX-…`: replace it from a PST.
[^6]: Over 25 MB, the message goes first, then each file.
[^7]: Outlook.com refuses messages over about 40 MB.
[^8]: Fetched as MIME and created in one request: the server may refuse it (HTTP 413).
[^9]: `--after` for copies an earlier plan made, `--damaged` for those
    `ecm check-content` found.
[^10]: Only copies `ecm` created; others are left alone.
[^11]: Not copied: series exceptions, attendees of received invitations, secondary
    calendars.
[^12]: Microsoft To Do lists into Exchange tasks.
[^13]: Graph has no API for sticky notes; M9b tries a message of class
    `IPM.StickyNote`.
[^14]: Kept by the server, not in a PST.
[^15]: Client-only rules come without conditions or actions: recreate them by hand.
[^16]: If Graph allows it: no API known.
[^17]: Neither EWS nor Graph exposes them: set them in the web client.
