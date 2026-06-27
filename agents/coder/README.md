# Coder Agent

A multi-stage coding agent with structured context management, designed for building features with human oversight.

## Stages

1. **analyze** (Autonomous) - Understand requirements and create implementation plan
2. **implement** (Autonomous) - Write code following the plan
3. **review** (Interactive) - Human review before finalizing changes

## Usage

```bash
lev spawn coder --task "Add authentication to the user API"
```

## Context Layout

- **architecture** (Pinned, 4K) - System architecture and design patterns
- **task** (Pinned, 2K) - Current task description and requirements
- **codebase** (Temporary, 30K) - Relevant code files
- **conversation** (SlidingWindow, 15K) - Recent discussion (last 20 messages)
- **implementation** (Compacting, 40K) - Active coding workspace
- **history** (CompactHistory, 8K) - Compressed implementation history
- **scratch** (Clearable, 10K) - Temporary calculations

## Features

- **Structured stages** for clear separation of planning, implementation, and review
- **SlidingWindow** for conversation history (keeps context focused)
- **Compacting regions** for large codebases (auto-summarizes when threshold hit)
- **Interactive review** stage for human approval before finalizing
