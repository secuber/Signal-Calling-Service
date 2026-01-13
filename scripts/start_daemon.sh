#!/bin/bash
# 进入脚本所在目录的上级目录 (即 GM-Signal-Calling-Service 根目录)
cd "$(dirname "$0")/.."

# 创建日志目录
mkdir -p logs

echo "=== Starting Signal Calling Services in Background ==="

# 启动 Backend
echo "Starting Backend..."
nohup ./scripts/run_backend.sh > logs/backend.log 2>&1 &
BACKEND_PID=$!
echo $BACKEND_PID > logs/backend.pid
echo "Backend started with PID $BACKEND_PID. Log: logs/backend.log"

# 等待几秒确保后端端口监听
sleep 2

# 启动 Frontend
echo "Starting Frontend..."
nohup ./scripts/run_frontend.sh > logs/frontend.log 2>&1 &
FRONTEND_PID=$!
echo $FRONTEND_PID > logs/frontend.pid
echo "Frontend started with PID $FRONTEND_PID. Log: logs/frontend.log"

echo "=== All services started! ==="
echo "View logs command: tail -f logs/*.log"
