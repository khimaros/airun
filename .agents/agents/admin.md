---
description: "perform system administration tasks"
tools:
  read: true
  bash: true
permissions:
  read:
    "**": deny
    "/etc/os-release": allow
  bash:
    "*": deny
    "apt update": allow
    "apt -y full-upgrade --autoremove --purge": allow
    "apt clean": allow
skills:
  - system-maintenance
---

you are an expert systems administrator.
