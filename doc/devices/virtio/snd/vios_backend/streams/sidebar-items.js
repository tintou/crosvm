initSidebarItems({"enum":[["StreamMsg","Messages that the worker can send to the stream (thread)."]],"fn":[["reply_control_op_status","Encapsulates sending the virtio_snd_hdr struct back to the driver."],["reply_pcm_buffer_status","Encapsulates sending the virtio_snd_pcm_status struct back to the driver."],["try_set_real_time_priority","Attempts to set the current thread’s priority to a value hight enough to handle audio IO. This may fail due to insuficient permissions."],["vios_error_to_status_code","Gets the appropriate virtio-snd error to return to the driver from a VioSError."]],"struct":[["Stream",""],["StreamProxy","Basically a proxy to the thread handling a particular stream."]]});