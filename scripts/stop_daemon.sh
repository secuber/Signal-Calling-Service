#!/bin/bash
cd "$(dirname "$0")/.."

echo "=== Stopping Signal Calling Services ==="

if [ -f logs/frontend.pid ]; then
    PID=$(cat logs/frontend.pid)
    if kill -0 $PID 2>/dev/null; then
        kill $PID
        echo "Frontend (PID $PID) stopped."
    else
        echo "Frontend process (PID $PID) not found."
    fi
    rm logs/frontend.pid
else
    echo "No frontend.pid found."
fi

if [ -f logs/backend.pid ]; then
    PID=$(cat logs/backend.pid)
    if kill -0 $PID 2>/dev/null; then
        kill $PID
        echo "Backend (PID $PID) stopped."
    else
        echo "Backend process (PID $PID) not found."
    fi
    rm logs/backend.pid
else
    echo "No backend.pid found."
fi

# Force kill any remaining binaries by name, just in case
pkill -f "calling_backend" && echo "Terminated remaining calling_backend processes."
pkill -f "calling_frontend" && echo "Terminated remaining calling_frontend processes."

echo "=== Services stopped ==="

echo "=== Services stopped ==="
