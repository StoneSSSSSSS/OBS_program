import subprocess
import config
def start_obs():
    OBS_PATH = config.OBS_PATH
    OBS_DIR  = config.OBS_DIR

    subprocess.Popen(
        [OBS_PATH, "--startrecording", "--startreplaybuffer", "--minimize-to-tray"],
        cwd=OBS_DIR
    )