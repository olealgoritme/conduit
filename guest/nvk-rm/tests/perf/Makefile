# vk_perf_bench: GPU time per workload category (NVK on RM vs NVIDIA)
#   make && VK_DRIVER_FILES=<icd.json> ./vk_perf_bench [-r reps] [-t alu,tex,...]
# vk_gpu_bound: GPU-bound throughput per stage (see its header)
#   make vk_gpu_bound && VK_DRIVER_FILES=<icd.json> ./vk_gpu_bound [-t alu,fill,...]
CC ?= cc

all: vk_perf_bench vk_gpu_bound

vk_perf_bench: vk_perf_bench.c shaders/.stamp
	$(CC) -O2 -Wall -o $@ vk_perf_bench.c -lvulkan

shaders/.stamp: gen_shaders.py
	python3 gen_shaders.py
	touch $@

vk_gpu_bound: vk_gpu_bound.c shaders_gb/.stamp
	$(CC) -O2 -Wall -o $@ vk_gpu_bound.c -lvulkan -lm

shaders_gb/.stamp: gen_gpu_bound_shaders.py
	python3 gen_gpu_bound_shaders.py
	touch $@

clean:
	rm -rf vk_perf_bench vk_gpu_bound shaders shaders_gb
