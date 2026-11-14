# Clean the old logs

Make a folder with a few logs.

```bash [name:setup]
export DEMO=$(mktemp -d)
cd "$DEMO" && seq 5000 > app.log && seq 900 > db.log
```

List the logs that the last step removes.

```bash [name:list]
ls -sh *.log
```

Destructive: remove them.

```bash [name:clean]
rm -v *.log
```
