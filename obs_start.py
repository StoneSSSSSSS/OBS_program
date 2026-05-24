import subprocess
import config
def start_obs():
    OBS_PATH = config.OBS_PATH
    OBS_DIR  = config.OBS_DIR

    print('Starting OBS...')
    subprocess.Popen(
        [OBS_PATH, "--startrecording", "--startreplaybuffer", "--minimize-to-tray", "--disable-shutdown-check","--disable-updater"],
        cwd=OBS_DIR
    )
    print('Obs started!')