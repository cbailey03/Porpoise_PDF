# Goals

We want to build a world class PDF viewer and editor with Rust, focused on maximum efficiency and security. We maintain complete test coverage as the program grows — a feature is not finished until it is tested. Every feature must also be controllable programmatically, without exception, so that an AI agent can drive the entire program on a user's behalf; designing for agent control matters, and it has to shape each feature as it is built rather than be bolted on afterwards.

## Goal 1: Single PDF Viewer

Allow a person to view a single PDF, with the ability to:
- Scroll page by page
- Free scroll (continuous scrolling)

Plan: [docs/goal-1-plan.md](docs/goal-1-plan.md)

## Goal 2: Complete Programmatic Control

Allow an AI agent to drive the whole program, with:
- Every effect reachable by a named command, so nothing is click-only
- A way to read the program's state, so an agent can see what its commands did
- An opt-in channel for an outside process to send commands and receive events

The user interface becomes one way to issue commands rather than a privileged
path into the program.

Plan: [docs/goal-2-plan.md](docs/goal-2-plan.md)
