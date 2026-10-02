# looper

Run [Claude Code](https://claude.com/claude-code) once per task, in order,
unattended. You write a list of tasks in a plan file; looper sends each one
to `claude -p` wrapped in a shared prefix and suffix, keeps a transcript of
every run, and collects the follow-up tasks Claude leaves behind.

## Screenshot

<img width="1317" height="632" alt="Screenshot 2026-09-27 at 20 11 30" src="https://github.com/user-attachments/assets/ff92eb37-e557-438f-bd85-36c7b50fed79" />

## Install

Requires Rust (edition 2024) and `claude` on your `PATH`.

```sh
./release.sh
```

This builds a release binary and installs it to `~/.local/bin/looper`.
`cargo install --path .` works too.

## Usage

```sh
looper plan new NAME              # create .looper/plans/NAME.toml
$EDITOR .looper/plans/NAME.toml   # write your tasks
looper plan run NAME              # run them, one claude call per task
```

### Plan files

```toml
# Flags passed to every `claude` call. The prompt is sent on stdin.
claude_args = ["-p", "--permission-mode", "auto"]

# Shell command run before every task (optional).
before_task = "git pull --ff-only"

# Text added before every task.
prefix = """
/goal
"""

# Text added after every task. {{plan}} is replaced with the plan's name.
suffix = """
Commit your work in small, focused commits. Run the tests. Write follow-ups
you didn't finish to .looper/tasks/{{plan}}/. ...
"""

# Each task becomes one `claude` call: prefix + task + suffix. Each task
# starts with a fresh context, so keep work that needs the same context together.
tasks = [
  """
  Add a --json flag to the list command.
  """,
  """
  Rename the Config struct to Settings.
  """,
]
```

`before_task` is run with `sh -c` before each task, from the project folder,
with `LOOPER_PLAN`, `LOOPER_TASK`, `LOOPER_TOTAL` and (unless `--no-log`)
`LOOPER_LOG_DIR` set. Its output is shown and saved in the task's log. If it
fails, looper warns and runs the task anyway; only Claude's result decides
whether the task failed. `{{plan}}` isn't replaced in it; use `$LOOPER_PLAN`.

`looper plan new` starts each plan with a copy of `.looper/config.toml`, which
is created with the defaults the first time `.looper/` is made. Edit it to
change the `claude_args`, `before_task`, `prefix` and `suffix` of future
plans; existing plans keep their own copy. The defaults include a suffix that
asks Claude to work without asking questions, commit as it goes, run the
tests, and write any follow-ups it didn't finish as markdown files in
`.looper/tasks/{{plan}}/`.
`looper plan run` refuses to start while a task still says `REPLACE ME`.

In the prefix, suffix and tasks, `{{plan}}` is replaced with the plan's name
(its file name without `.toml`), so each plan's follow-ups get a folder of their
own and runs of different plans don't mix.

### Running

```sh
looper plan list              # plans, most recently changed first
looper plan show PLAN         # a plan's claude flags, prefix, tasks and suffix
looper plan run PLAN          # run every task in a plan
looper plan stop PLAN         # stop a run once its current task is done
looper plan delete PLAN...    # delete plans
```

`PLAN` is a plan name from `.looper/plans/`, or a path to a plan file.

| Flag                | Effect                                                     |
| ------------------- | ---------------------------------------------------------- |
| `--dry-run`         | Print the prompts without running them                     |
| `--stop-on-failure` | Stop at the first failing task instead of continuing       |
| `--log-dir DIR`     | Save transcripts under `DIR` instead of `.looper/logs`     |
| `--no-log`          | Don't save transcripts; only show Claude's final replies   |

The plan is read again before every task, so you can change it while it runs:
add, remove, reorder or rewrite tasks, or change the flags, prefix, suffix or
`before_task`. The next task is the first one in the plan that hasn't run yet,
matched by its text, so editing a task that already ran makes it run again.
If the plan doesn't load, for example because it's half-saved, looper warns
and keeps the last version. A task that says `REPLACE ME` stops the run there.

To end a run without cutting a task short, run `looper plan stop PLAN` from
another terminal. It creates `PLAN.stop` next to the plan file; the status line
then says the run is stopping, and the run ends when the current task is done
instead of starting the next one. Deleting the file before then takes the stop
back. A stop file left from an earlier run is removed when a run starts.

While it runs, looper shows a readable version of the conversation with a
timestamp on each line, and a status line at the bottom with the current task,
how many are left, elapsed time and tokens used. A short title for each task is
generated in the background with Haiku. At the end it prints the number of tasks
run, the models used, tokens (input, output and cache reads and writes) and
cost, and which tasks failed (exiting with status 1 if any did).

### Logs

Each run gets its own folder, `.looper/logs/<plan>-<timestamp>/`, with one
`task-NN.jsonl` per task holding Claude's full `stream-json` output plus
looper's own start, title and exit events.

```sh
looper log list               # runs, newest first
looper log list RUN           # tasks of one run
looper log show [RUN]         # transcript of a run (default: the latest)
looper log show --task 2      # only task 2
looper log show --detail minimal|compact|normal|full
looper log delete RUN...      # delete runs
looper log clean              # delete all logs
```

Pass `--log-dir DIR` to read logs saved with `looper plan run --log-dir DIR`.

### Follow-up tasks

```sh
looper task list [PLAN]       # follow-ups Claude wrote, newest first
looper task show [TASK]       # one task (number, file name or plan/file) or all
looper task delete TASK...    # delete tasks (numbers or file names)
looper task clean [PLAN]      # delete them all, or those of one plan
```

Tasks are read from `.looper/tasks/<plan>/`, and from `.looper/tasks/` itself
for plans whose suffix has no `{{plan}}`. `list` shows which plan each task
belongs to; with `PLAN` it keeps the same numbers as the full list, so they
work with `show`. Pass `--task-dir DIR` to read tasks from another folder.

`show` commands go through `$PAGER` (default `less`) when writing to a
terminal; pass `--no-pager` to print directly. Colors follow `NO_COLOR` and
`CLICOLOR_FORCE`.

## The .looper folder

Every command uses the `.looper/` folder in the current directory or the
nearest parent directory that has one, so you can run looper from anywhere in
the project. Without one, `looper plan new` creates it in the current directory.
`looper plan run` starts Claude in the folder that holds `.looper/`.

`.looper/` gets a `.gitignore` that ignores everything in it, so Claude's
commits never pick up plans, logs or follow-up tasks. A custom `--log-dir` gets
its own `.gitignore` for the same reason.
