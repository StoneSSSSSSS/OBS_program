import pygetwindow as gw
from os import listdir, mkdir, remove
from shutil import move
from _thread import start_new_thread
import keyboard
import time
from bug_log import add_to_bug_log
from overlay import clippled_overlay
from overlay import overlay_thread_function


path_to_recording = "I:\OBS\Recordings"
path_to_clip = "I:\OBS\Clips"

def get_active_window_title():
    name = gw.getActiveWindow().title
    if not name:
        name = "Desktop"
    return name

queue = []

def add_to_queue(recording):
    queue.append(recording)

def process_queue():
    global queue
    while True:
        try:
            if queue:
                recording = queue[0]
                remove(path_to_recording + "\\" + recording)
                queue.pop(0)
        except PermissionError as e:
            pass
        time.sleep(5)


def remove_oldest_recordings():
    while True:
        recordings = listdir(path_to_recording)
        recordings = [x for x in recordings if "Replay" not in x]
        if len(recordings) > 48:
            number_above = len(recordings) - 48
            for i in range(number_above):
                if recordings[i] not in queue:
                    try:
                        remove(path_to_recording + "\\" + recordings[i])
                    except PermissionError as e:
                        add_to_queue(recordings[i])

        time.sleep(30)

def hotkey_action():
    print("Hotkey pressed")

    active_window = get_active_window_title()
    max_time = 10
    start_time = time.time()
    while True:
        recordings = listdir(path_to_recording)
        recordings = [x for x in recordings if "Replay" in x]
        if recordings:
            break
        if time.time() - start_time > max_time:
            add_to_bug_log("No recordings found")
            return
    print("overlay")
    clippled_overlay()
    move_recording(recordings,active_window)

def move_recording(recordings,active_window):
    # move to clips in a folder named after the active window
    all_invalid_chars = ["/", "\\", ":", "*", "?", "\"", "<", ">", "|", "."]
    for char in all_invalid_chars:
        active_window = active_window.replace(char, "")
    active_window = active_window.replace(" ", "_")
    # create folder with name of active window if it doesn't exist
    if active_window not in listdir(path_to_clip):
        try:
            path = path_to_clip + "\\" + active_window
            mkdir(path)
        except Exception as e:
            add_to_bug_log(e)
            return
    # move replay to folder
    time.sleep(5)
    for recording in recordings:
        old_path = path_to_recording + "\\" + recording
        new_path = path_to_clip + "\\" + active_window + "\\" + recording

        file_in_use = True
        while file_in_use:
            try:
                move(old_path, new_path)
                file_in_use = False
            except Exception as e:
                add_to_bug_log(e)
                time.sleep(5)
        print(f"Moving {old_path} to {new_path}")


def clip_hotkey_listener():
    print("Listening for hotkey")
    # Register the hotkey and specify the callback function
    keyboard.add_hotkey('ctrl+shift+`', hotkey_action)
    # Keep the listener active
    keyboard.wait()

start_new_thread(process_queue, ())

if __name__ == "__main__":
    start_new_thread(overlay_thread_function, ())
    start_new_thread(clip_hotkey_listener, ())
    while True:
        time.sleep(10)
