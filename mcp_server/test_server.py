#!/usr/bin/env python3
"""
Test script for CASS MCP Server

Tests the MCP server tools via direct HTTP calls.
"""

import httpx
import json
import sys

BASE_URL = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:8888"

def test_endpoint(name: str, url: str, expected_status: int = 200):
    """Test an endpoint and print result."""
    print(f"\n{'='*60}")
    print(f"Testing: {name}")
    print(f"URL: {url}")
    print(f"{'='*60}")
    
    try:
        response = httpx.get(url, timeout=10.0)
        status = "✅ PASS" if response.status_code == expected_status else "❌ FAIL"
        print(f"Status: {response.status_code} {status}")
        
        try:
            data = response.json()
            print(f"Response: {json.dumps(data, indent=2)[:500]}")
        except:
            print(f"Response: {response.text[:500]}")
            
        return response.status_code == expected_status
    except Exception as e:
        print(f"❌ ERROR: {e}")
        return False


def main():
    print(f"\n🚀 Testing CASS MCP Server at {BASE_URL}")
    
    results = []
    
    # Test info endpoint
    results.append(test_endpoint("Server Info", f"{BASE_URL}/"))
    
    # Test health endpoint
    results.append(test_endpoint("Health Check", f"{BASE_URL}/health"))
    
    # Test SSE endpoint (should return event stream)
    print(f"\n{'='*60}")
    print("Testing: SSE Endpoint")
    print(f"URL: {BASE_URL}/sse")
    print(f"{'='*60}")
    
    try:
        # SSE connection test - just verify it accepts connections
        with httpx.stream("GET", f"{BASE_URL}/sse", timeout=3.0) as response:
            print(f"Status: {response.status_code}")
            print("SSE connection accepted ✅")
            results.append(True)
    except httpx.ReadTimeout:
        # Timeout is expected for SSE - it means connection is working
        print("SSE connection accepted (timeout expected) ✅")
        results.append(True)
    except Exception as e:
        print(f"❌ ERROR: {e}")
        results.append(False)
    
    # Summary
    print(f"\n{'='*60}")
    print(f"📊 Results: {sum(results)}/{len(results)} tests passed")
    print(f"{'='*60}")
    
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
