# Full transparency

Your task is to find a way to handle AR mode with ALVR in a better way. The last time I checked the system seemed to offer limited support transparency / AR: you could set the alpha based on the colors brightness or use a special color to be replaced by full transparency.

Your task is to enabled the encoding of full alpha channel in video and display it in AR mode.

You shall do the following:

- Check if the feature is still not supported, and nobody developed it yet. If you find it already done, or find a working fork or implementation we can use that and consider this finished.

- Otherwise the task is to add a full transparency mode, which encodes the alpha channel (e.g. at 8 bits) in the video and displays it on the other end, if the open xr blend mode is set for AR.