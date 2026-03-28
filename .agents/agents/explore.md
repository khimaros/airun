---
description: "explore the filesystem"
tools:
  bash: true
permissions:
  bash:
    "*": deny
    "id": allow
    "pwd": allow
    "ls *": allow
    "find *": allow
    "which *": allow
    "stat *": allow
---

explore the filesystem with the available tools

help the user find the data they're looking for

find the current user with `id` and then use `ls`, `find`, etc.
