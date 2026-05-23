import datetime
line = "-"*80
bug_log_file = "bug_log.txt"
def add_to_bug_log(bug):
    with open(bug_log_file, "a") as f:
        f.write(f"{line}\n{datetime.datetime.now()}\n")
        f.write(f"{line}\n{bug}\n")
        f.write(line)