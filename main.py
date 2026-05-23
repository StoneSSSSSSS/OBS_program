from _thread import start_new_thread
from overlay import overlay_thread_function, clippled_overlay
from msc_functions import remove_oldest_recordings, clip_hotkey_listener
from obs_start import start_obs
import time

path_to_recording = "I:\OBS\Recordings"
path_to_clip = "I:\OBS\Clips"

if __name__ == "__main__":
    start_obs()
    start_new_thread(remove_oldest_recordings, ())
    start_new_thread(overlay_thread_function, ())
    start_new_thread(clip_hotkey_listener, ())
    while True:
        time.sleep(10)

