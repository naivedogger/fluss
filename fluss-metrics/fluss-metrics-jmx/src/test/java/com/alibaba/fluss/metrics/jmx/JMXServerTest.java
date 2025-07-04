/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *    http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package com.alibaba.fluss.metrics.jmx;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import javax.management.InstanceNotFoundException;
import javax.management.MBeanServer;
import javax.management.MBeanServerConnection;
import javax.management.ObjectName;
import javax.management.remote.JMXConnector;
import javax.management.remote.JMXConnectorFactory;
import javax.management.remote.JMXServiceURL;

import java.lang.management.ManagementFactory;
import java.util.Optional;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

/** Test for {@link JMXServer} functionality. */
class JMXServerTest {

    @BeforeEach
    void setUp() {
        JMXService.startInstance("23456-23466");
    }

    @AfterEach
    void tearDown() throws Exception {
        JMXService.stopInstance();
    }

    /** Verifies initialize, registered mBean and retrieval via attribute. */
    @Test
    void testJMXServiceRegisterMBean() throws Exception {
        TestObject testObject = new TestObject();
        ObjectName testObjectName = new ObjectName("com.alibaba.fluss.jmx", "key", "value");
        MBeanServer mBeanServer = ManagementFactory.getPlatformMBeanServer();

        try {
            Optional<JMXServer> server = JMXService.getInstance();
            assertThat(server).isPresent();
            mBeanServer.registerMBean(testObject, testObjectName);

            JMXServiceURL url =
                    new JMXServiceURL(
                            "service:jmx:rmi://localhost:"
                                    + server.get().getPort()
                                    + "/jndi/rmi://localhost:"
                                    + server.get().getPort()
                                    + "/jmxrmi");
            JMXConnector jmxConn = JMXConnectorFactory.connect(url);
            MBeanServerConnection mbeanConnConn = jmxConn.getMBeanServerConnection();

            assertThat((int) mbeanConnConn.getAttribute(testObjectName, "Foo")).isOne();
            mBeanServer.unregisterMBean(testObjectName);
            assertThatThrownBy(() -> mbeanConnConn.getAttribute(testObjectName, "Foo"))
                    .isInstanceOf(InstanceNotFoundException.class);
        } finally {
            JMXService.stopInstance();
        }
    }

    /** Test MBean interface. */
    public interface TestObjectMBean {
        int getFoo();
    }

    /** Test MBean Object. */
    public static class TestObject implements TestObjectMBean {

        @Override
        public int getFoo() {
            return 1;
        }
    }
}
