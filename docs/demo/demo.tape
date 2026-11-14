# Records docs/demo.gif, the demo of the README: mise run demo.
#
# An agent adds a step to the runbook while it runs: a background job
# copies clean-logs-v2.md over clean-logs.md after the list block ends.

Output docs/demo.gif

Set Shell bash
Set FontFamily "JetBrainsMono NFM"
Set FontSize 16
Set Width 1000
Set Height 720
Set Padding 16
Set Theme GruvboxDark
Set TypingSpeed 50ms

Hide
Type "export LC_ALL=C.UTF-8 PATH=$PWD/target/release:$PATH src=$PWD/docs/demo; cd $(mktemp -d) && cp $src/clean-logs.md . && ( (until grep -qs '^=== END list' clean-logs.log; do sleep 0.1; done; sleep 1.5; cp $src/clean-logs-v2.md clean-logs.md) & ) && clear"
Enter
Sleep 500ms
Show

Type "stepbook clean-logs.md"
Sleep 600ms
Enter
Wait+Screen /NORMAL/
Sleep 1.5s

# setup: the export and the cd carry over to the next blocks.
Enter
Wait+Screen /setup +✔/
Sleep 1s

# list: its output shows under its code.
Type "j"
Sleep 500ms
Enter
Wait+Screen /K app\.log/
Sleep 1.5s

# The agent adds the archive step, and stepbook reloads the file.
Wait+Screen /archive/
Sleep 2s

Type "j"
Sleep 500ms
Enter
Wait+Screen /archive +✔/
Sleep 1.5s

Type "j"
Sleep 500ms
Enter
Wait+Screen /removed 'db\.log'/
Sleep 3s
