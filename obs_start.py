import subprocess
def start_obs():
    OBS_PATH = r"C:\Program Files\obs-studio\bin\64bit\obs64.exe"
    OBS_DIR  = r"C:\Program Files\obs-studio\bin\64bit"

    subprocess.Popen(
        [OBS_PATH, "--startrecording", "--startreplaybuffer", "--minimize-to-tray"],
        cwd=OBS_DIR
    )