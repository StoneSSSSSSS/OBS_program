with open("configs.txt", "r") as f:
    print(f.read)
    configs = f.read().split("\n")

for i,config in enumerate(configs):
    config = config.split('=')
    config = config[1:]
    config = "".join(config)
    configs[i] = config

overlay_image_name = configs[0]
overlay_hotkey = configs[1]
OBS_PATH = configs[2]
OBS_DIR = '\\'.join(configs[2].split('\\')[:-1])
path_to_recording = configs[3]
path_to_clip = configs[4]