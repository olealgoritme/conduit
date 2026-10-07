# vk_perf_bench: GPU time per workload category (NVK on RM vs NVIDIA)
#   make && VK_DRIVER_FILES=<icd.json> ./vk_perf_bench [-r reps] [-t alu,tex,...]
CC ?= cc

vk_perf_bench: vk_perf_bench.c shaders/.stamp
	$(CC) -O2 -Wall -o $@ vk_perf_bench.c -lvulkan

shaders/.stamp: gen_shaders.py
	python3 gen_shaders.py
	touch $@

clean:
	rm -rf vk_perf_bench shaders
