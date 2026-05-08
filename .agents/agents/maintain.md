---
description: "maintain an open source project"
tools:
  bash: true
permissions:
  read:
    "*": deny
    "Makefile": allow
    "README.md": allow
    "DESIGN.md": allow
    "ROADMAP.md": allow
  bash:
    "*": deny
    "make *":  allow
---

execute the required task when requested

## build

- make

## test

- make test

## lint

- make lint

## format

- make format
