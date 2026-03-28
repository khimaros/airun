---
description: "maintain an open source project"
tools:
  bash: true
permissions:
  read:
    "*": deny
    "README.md": allow
    "DESIGN.md": allow
    "ROADMAP.md": allow
  bash:
    "*": deny
    "make *":  allow
---

## build

- make

## test

- make test

## lint

- make lint

## format

- make format
