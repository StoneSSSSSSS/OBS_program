import threading
import tkinter as tk
from PIL import Image, ImageTk
from _thread import start_new_thread
import time
import config

overlay_active = False  # Global state to track overlay visibility

def overlay_thread_function():
    """
    This function runs in a separate thread, managing the Tkinter window.
    It periodically checks 'overlay_active' to show/hide the overlay.
    """
    root = tk.Tk()
    root.overrideredirect(True)  # Remove window decorations
    root.attributes("-topmost", True)  # Keep the window on top

    # Load the image
    image = Image.open(config.overlay_image_name)
    img_width, img_height = image.size
    photo = ImageTk.PhotoImage(image)

    label = tk.Label(root, image=photo, bg="white")
    label.pack()

    # Temporarily position the window off-screen to avoid flicker
    # then withdraw until it is toggled on.
    root.geometry(f"{img_width}x{img_height}+{-9999}+{-9999}")
    root.withdraw()

    def position_overlay():
        screen_width = root.winfo_screenwidth()
        # Place overlay near top-right corner (adjust for your needs)
        x_position = screen_width - img_width - 10
        y_position = 10
        root.geometry(f"{img_width}x{img_height}+{x_position}+{y_position}")

    # This function runs in its own thread, polling overlay_active
    # to decide whether to show or hide the overlay.
    def poll_overlay():
        while True:
            if overlay_active:
                # Position overlay before showing
                position_overlay()
                root.deiconify()  # Show the window
            else:
                root.withdraw()  # Hide the window
            time.sleep(0.2)  # Poll every 0.2 seconds

    # Start a polling thread to check overlay_active continuously
    poll_thread = threading.Thread(target=poll_overlay, daemon=True)
    poll_thread.start()

    root.mainloop()

def start_overlay_thread(image_path):
    """
    Start the thread containing the overlay.
    This should typically be called once in your application.
    """
    thread = threading.Thread(target=overlay_thread_function, args=(image_path,), daemon=True)
    thread.start()

def toggle_overlay():
    """
    Toggle the global 'overlay_active' flag to show or hide the overlay.
    """
    global overlay_active
    overlay_active = not overlay_active
    print(f"Overlay active: {overlay_active}")

def clippled_overlay():
    toggle_overlay()
    time.sleep(5)
    toggle_overlay()

# Example usage:
if __name__ == "__main__":
    start_new_thread(overlay_thread_function, ())

    while True:
        time.sleep(3)
        toggle_overlay()