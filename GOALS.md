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

## Goal 3: Open a File From Inside the Program

Let a person open a PDF without typing a path, with:
- An **Open…** button and shortcut that bring up the system file dialog
- A window that starts empty and waits, rather than refusing to launch with no path
- A visible message when a file cannot be opened, instead of a silent failure

The dialog is a way of *choosing an argument* for the existing `open` command, not a
new capability — so an agent gains nothing it did not already have.

Plan: [docs/goal-3-plan.md](docs/goal-3-plan.md)

## Goal 4: Reorganize Pages and Save

Let a person change the order of a document's pages and keep the result, with:
- Moving a page to a different position, and deleting a page
- Undo, so a mistake costs nothing
- **Save** over the original, and **Save As** to a new file

This is the first thing that writes to disk, so a save must either produce a
complete correct file or leave the original untouched — never something in between.

Plan: [docs/goal-4-plan.md](docs/goal-4-plan.md)

## Goal 5: Merge PDFs

Let a person combine multiple PDFs into one document.

Plan: [docs/goal-5-plan.md](docs/goal-5-plan.md)
