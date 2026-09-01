---
name: kayak-flight-search
description: "Search flights on KAYAK using browser tool; LATAM and Avianca block bots"
metadata: {"nanobot":{"always":false}}
---

# KAYAK Flight Search (Browser Tool)

Use the browser tool to search flights on KAYAK — it loads successfully.

**URL pattern:**
```
https://www.kayak.com/flights/PEI-BOG/2026-07-24?sort=price_a
```
Replace `PEI-BOG` with any airport pair, and the date as needed.

**What works:**
- KAYAK loads fine in the browser tool

**What blocks bots:**
- LATAM (latam.com)
- Avianca (avianca.com)
